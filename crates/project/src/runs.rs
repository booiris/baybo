//! The predicates a run has to satisfy, the ledger entry it becomes, and
//! which of a card's earlier runs this one is a continuation of.

use baybo_model::{AgentFramework, AgentProfileId, IssueRunId};
use baybo_store::project::{
    IssueRow, IssueRunRow, IssueStatus, NewIssueRun, RunStatus, RunTrigger,
};

/// How a run ended, as the executor that ran it saw it.
///
/// The executor decides these — only it watched the turn — and the board
/// decides what they cost the card. `stopped_by_a_human` is separate from
/// `status` because the ledger row cannot carry it and it changes what the
/// board owes: a person who pressed stop is not asking for a follow-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    pub status: RunStatus,
    pub error: Option<String>,
    pub stopped_by_a_human: bool,
}

/// What a write did to an issue, in the only terms [`triggers_run`] cares
/// about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Transition {
    pub before_status: IssueStatus,
    pub before_assignee: Option<AgentProfileId>,
    pub after_status: IssueStatus,
    pub after_assignee: Option<AgentProfileId>,
}

/// Whether this transition starts work, and why.
pub(crate) fn triggers_run(t: Transition) -> Option<RunTrigger> {
    if t.after_status != IssueStatus::InProgress || t.after_assignee.is_none() {
        return None;
    }
    if t.before_status != IssueStatus::InProgress {
        return Some(RunTrigger::Started);
    }
    // Who is on it, not merely whether somebody is: handing a live card to
    // a different agent is a handover, and a handover nobody starts leaves
    // the board showing @dev-2 on work only @dev-1 ever touched.
    (t.before_assignee != t.after_assignee).then_some(RunTrigger::Assigned)
}

/// Whether this card still takes work at all.
pub(crate) fn accepts_runs(issue: &IssueRow) -> bool {
    !crate::stages::is_finished(issue)
}

/// Whether an agent on this framework can host an issue's session.
pub fn can_host_a_session(framework: AgentFramework) -> bool {
    framework == AgentFramework::Baybo
}

impl Transition {
    /// The transition between two states of the same issue.
    pub(crate) fn between(before: &IssueRow, after: &IssueRow) -> Self {
        Self {
            before_status: before.status,
            before_assignee: before.assignee.clone(),
            after_status: after.status,
            after_assignee: after.assignee.clone(),
        }
    }

    /// A newly created issue: it came from nowhere, so the "before" is a
    /// state that can never itself be a trigger.
    pub(crate) fn created(after: &IssueRow) -> Self {
        Self {
            before_status: IssueStatus::Backlog,
            before_assignee: None,
            after_status: after.status,
            after_assignee: after.assignee.clone(),
        }
    }
}

/// Whether this run ever got as far as being picked up — a name for
/// [`IssueRunRow::was_claimed`], which is where the rule lives.
pub(crate) fn ever_ran(run: &IssueRunRow) -> bool {
    run.was_claimed()
}

pub(crate) fn is_unsettled(run: &IssueRunRow) -> bool {
    run.settled_at.is_none()
}

pub(crate) fn unsettled(runs: &[IssueRunRow]) -> Option<&IssueRunRow> {
    runs.iter().find(|run| is_unsettled(run))
}

/// The run whose session this one continues: the newest run of the same
/// agent's that actually executed. `None` opens a fresh session.
pub fn session_run_to_continue<'a>(
    run: &IssueRunRow,
    runs: &'a [IssueRunRow],
) -> Option<&'a IssueRunRow> {
    newest_run_that_ran(&run.agent_id, runs.iter())
}

/// [`session_run_to_continue`]'s rule over the runs *before* this one — the
/// run whose turn is already in the transcript this one opens, and so the
/// point the card's conversation is a delta from.
pub fn session_run_before<'a>(
    run: &IssueRunRow,
    runs: &'a [IssueRunRow],
) -> Option<&'a IssueRunRow> {
    newest_run_that_ran(
        &run.agent_id,
        runs.iter().filter(|candidate| candidate.id != run.id),
    )
}

fn newest_run_that_ran<'a>(
    agent: &AgentProfileId,
    runs: impl Iterator<Item = &'a IssueRunRow>,
) -> Option<&'a IssueRunRow> {
    runs.filter(|candidate| &candidate.agent_id == agent && ever_ran(candidate))
        .max_by_key(|candidate| candidate.attempt)
}

/// Build the ledger entry for a triggered run.
///
/// `agent` rather than `issue.assignee`: a triage run is recorded on a card
/// with nobody on it, and the row still has to name who is being asked.
pub(crate) fn ledger_entry(
    issue: &IssueRow,
    trigger: RunTrigger,
    agent: AgentProfileId,
) -> NewIssueRun {
    NewIssueRun {
        id: IssueRunId::generate(),
        issue_id: issue.id.clone(),
        project_id: issue.project_id.clone(),
        number: issue.number,
        agent_id: agent,
        trigger,
    }
}

/// Maximum boot redispatches; no timer exists for delayed recovery.
const MAX_RUN_RESUMES: i64 = 2;

/// Maximum unclaimed wait, excluding known block time; holds lack a release timestamp.
const MAX_QUEUED_WAIT_HOURS: i64 = 48;

const GAVE_UP_RESUMING: &str = r#"this run was interrupted and handed back out too many times without ever finishing, so the board stopped resuming it — retry the card once you know why"#;

const WAITED_TOO_LONG: &str = r#"this run waited too long to be picked up and never was; the card has moved on since — retry it if the work is still wanted"#;

const PARKED_BY_A_BLOCK: &str = "the card is blocked; leaving its recorded run parked";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GaveUpOn {
    Resumes,
    QueuedWait,
}

/// Recovery verdict for a previously recorded row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    Stands,
    Park(&'static str),
    CallOff,
    GiveUp {
        bound: GaveUpOn,
        reason: &'static str,
    },
}

/// Decide whether a recorded row may run now.
pub(crate) fn verdict(
    run: &IssueRunRow,
    card: Option<&IssueRow>,
    now: chrono::DateTime<chrono::Utc>,
) -> Verdict {
    let Some(card) = card.filter(|card| accepts_runs(card)) else {
        return Verdict::CallOff;
    };
    if parked_by_a_block(run, card) {
        return Verdict::Park(PARKED_BY_A_BLOCK);
    }
    // Hold time belongs to the budget, not the queued-wait bound.
    if run.status != RunStatus::Queued {
        return Verdict::Stands;
    }
    if ever_ran(run) {
        if run.resumes > MAX_RUN_RESUMES {
            return Verdict::GiveUp {
                bound: GaveUpOn::Resumes,
                reason: GAVE_UP_RESUMING,
            };
        }
    } else if now.signed_duration_since(run.created_at.max(card.updated_at))
        > chrono::Duration::hours(MAX_QUEUED_WAIT_HOURS)
    {
        return Verdict::GiveUp {
            bound: GaveUpOn::QueuedWait,
            reason: WAITED_TOO_LONG,
        };
    }
    Verdict::Stands
}

fn survives_a_block(trigger: RunTrigger) -> bool {
    trigger == RunTrigger::Blocked
}

/// Whether a block parks this non-running row.
pub(crate) fn parked_by_a_block(run: &IssueRunRow, card: &IssueRow) -> bool {
    !crate::driver::board_may_start(card)
        && !survives_a_block(run.trigger)
        && run.status != RunStatus::Running
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str) -> Option<AgentProfileId> {
        Some(AgentProfileId::parse(id.to_owned()).expect("agent id"))
    }

    fn t(
        before_status: IssueStatus,
        before_assignee: Option<AgentProfileId>,
        after_status: IssueStatus,
        after_assignee: Option<AgentProfileId>,
    ) -> Transition {
        Transition {
            before_status,
            before_assignee,
            after_status,
            after_assignee,
        }
    }

    fn issue(status: IssueStatus) -> IssueRow {
        let now = chrono::Utc::now();
        IssueRow {
            id: baybo_model::IssueId::generate(),
            project_id: baybo_model::ProjectId::parse("p").expect("id"),
            number: 1,
            title: "Wire it".into(),
            description: String::new(),
            attachments: Vec::new(),
            status,
            priority: baybo_store::project::IssuePriority::None,
            assignee: agent("dev-1"),
            position: 0,
            blocked_reason: None,
            branch: None,
            parent_issue_id: None,
            stage: 0,
            source_key: None,
            cancelled_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn entering_in_progress_with_somebody_on_it_starts_work() {
        assert_eq!(
            triggers_run(t(
                IssueStatus::Todo,
                agent("dev-1"),
                IssueStatus::InProgress,
                agent("dev-1")
            )),
            Some(RunTrigger::Started)
        );
        for from in [IssueStatus::Backlog, IssueStatus::Review, IssueStatus::Done] {
            assert_eq!(
                triggers_run(t(
                    from,
                    agent("dev-1"),
                    IssueStatus::InProgress,
                    agent("dev-1")
                )),
                Some(RunTrigger::Started)
            );
        }
    }

    #[test]
    fn assigning_a_card_already_in_the_column_starts_work_too() {
        assert_eq!(
            triggers_run(t(
                IssueStatus::InProgress,
                None,
                IssueStatus::InProgress,
                agent("dev-1")
            )),
            Some(RunTrigger::Assigned)
        );
    }

    #[test]
    fn reassigning_in_the_column_hands_the_card_over() {
        assert_eq!(
            triggers_run(t(
                IssueStatus::InProgress,
                agent("dev-1"),
                IssueStatus::InProgress,
                agent("dev-2")
            )),
            Some(RunTrigger::Assigned)
        );
    }

    #[test]
    fn nothing_else_starts_work() {
        assert!(
            triggers_run(t(
                IssueStatus::InProgress,
                agent("dev-1"),
                IssueStatus::InProgress,
                agent("dev-1")
            ))
            .is_none()
        );
        assert!(
            triggers_run(t(
                IssueStatus::InProgress,
                agent("dev-1"),
                IssueStatus::Review,
                agent("dev-1")
            ))
            .is_none()
        );
        assert!(
            triggers_run(t(
                IssueStatus::InProgress,
                agent("dev-1"),
                IssueStatus::Review,
                agent("dev-2")
            ))
            .is_none()
        );
        assert!(
            triggers_run(t(
                IssueStatus::Done,
                agent("dev-1"),
                IssueStatus::Done,
                agent("dev-2")
            ))
            .is_none()
        );
        assert!(triggers_run(t(IssueStatus::Todo, None, IssueStatus::InProgress, None)).is_none());
        assert!(
            triggers_run(t(
                IssueStatus::Backlog,
                agent("dev-1"),
                IssueStatus::Todo,
                agent("dev-1")
            ))
            .is_none()
        );
    }

    #[test]
    fn a_card_the_board_has_finished_with_takes_no_more_runs() {
        let mut cancelled = issue(IssueStatus::InProgress);
        cancelled.cancelled_at = Some(chrono::Utc::now());
        assert!(!accepts_runs(&cancelled), "cancelled");
        assert!(!accepts_runs(&issue(IssueStatus::Done)), "done");
        let mut cancelled_in_review = issue(IssueStatus::Review);
        cancelled_in_review.cancelled_at = Some(chrono::Utc::now());
        assert!(!accepts_runs(&cancelled_in_review), "cancelled in Review");

        for status in [
            IssueStatus::Backlog,
            IssueStatus::Todo,
            IssueStatus::InProgress,
            IssueStatus::Review,
        ] {
            assert!(accepts_runs(&issue(status)), "{status:?}");
        }
    }

    fn run(status: RunStatus, trigger: RunTrigger) -> IssueRunRow {
        IssueRunRow {
            id: IssueRunId::generate(),
            issue_id: baybo_model::IssueId::generate(),
            project_id: baybo_model::ProjectId::parse("p").expect("id"),
            number: 1,
            agent_id: AgentProfileId::parse("dev-1".to_owned()).expect("agent"),
            session_id: None,
            trigger,
            status,
            attempt: 1,
            resumes: 0,
            error: None,
            created_at: chrono::Utc::now(),
            started_at: None,
            settled_at: None,
        }
    }

    fn claimed(mut run: IssueRunRow) -> IssueRunRow {
        run.session_id = Some(baybo_model::SessionId::new());
        run.started_at = Some(chrono::Utc::now());
        run
    }

    #[test]
    fn a_row_on_a_card_the_board_has_finished_with_is_called_off() {
        let now = chrono::Utc::now();
        let row = run(RunStatus::Queued, RunTrigger::Started);
        assert_eq!(
            verdict(&row, None, now),
            Verdict::CallOff,
            "a card that is gone takes no more work"
        );
        assert_eq!(
            verdict(&row, Some(&issue(IssueStatus::Done)), now),
            Verdict::CallOff
        );
        let mut cancelled = issue(IssueStatus::InProgress);
        cancelled.cancelled_at = Some(now);
        assert_eq!(verdict(&row, Some(&cancelled), now), Verdict::CallOff);
    }

    #[test]
    fn a_blocked_card_parks_its_work_run_and_not_the_leads_question() {
        let now = chrono::Utc::now();
        let mut card = issue(IssueStatus::InProgress);
        card.blocked_reason = Some("the goal contradicts the spec".into());

        let work = run(RunStatus::Queued, RunTrigger::Comment);
        assert_eq!(
            verdict(&work, Some(&card), now),
            Verdict::Park(PARKED_BY_A_BLOCK)
        );

        let question = run(RunStatus::Queued, RunTrigger::Blocked);
        assert_eq!(
            verdict(&question, Some(&card), now),
            Verdict::Stands,
            "the one wake that exists because of the block must not be parked by it"
        );
    }

    #[test]
    fn the_other_three_questions_are_parked_by_a_block_like_any_other_work() {
        let now = chrono::Utc::now();
        let mut card = issue(IssueStatus::InProgress);
        card.blocked_reason = Some("the operator paused this".into());

        for trigger in [RunTrigger::Review, RunTrigger::Stalled, RunTrigger::Triage] {
            let recorded = run(RunStatus::Queued, trigger);
            assert_eq!(
                verdict(&recorded, Some(&card), now),
                Verdict::Park(PARKED_BY_A_BLOCK),
                "{trigger:?} was recorded before the block and asks about something else; \
                 dispatching it puts the lead on a card somebody paused"
            );
            assert!(parked_by_a_block(&recorded, &card));
        }
    }

    #[test]
    fn a_run_already_executing_was_never_parked_by_the_block() {
        let mut card = issue(IssueStatus::InProgress);
        card.blocked_reason = Some("the agent blocked its own card mid-run".into());
        let running = claimed(run(RunStatus::Running, RunTrigger::Started));
        assert!(
            !parked_by_a_block(&running, &card),
            "the block landed under a turn that is still going; its executor settles it, \
             and counting it as parked would wake the lead on a card that is being worked"
        );
    }

    #[test]
    fn the_board_gives_up_after_the_resume_bound() {
        let now = chrono::Utc::now();
        let card = issue(IssueStatus::InProgress);

        let mut nearly = claimed(run(RunStatus::Queued, RunTrigger::Started));
        nearly.resumes = MAX_RUN_RESUMES;
        assert_eq!(
            verdict(&nearly, Some(&card), now),
            Verdict::Stands,
            "the bound is how many resurrections are allowed, and this is the last of them"
        );

        let mut past = nearly.clone();
        past.resumes = MAX_RUN_RESUMES + 1;
        assert_eq!(
            verdict(&past, Some(&card), now),
            Verdict::GiveUp {
                bound: GaveUpOn::Resumes,
                reason: GAVE_UP_RESUMING,
            }
        );
    }

    #[test]
    fn an_unclaimed_row_ages_out_and_a_claimed_one_does_not() {
        let now = chrono::Utc::now();
        let old_enough = now - chrono::Duration::hours(MAX_QUEUED_WAIT_HOURS + 1);
        let mut card = issue(IssueStatus::InProgress);
        card.updated_at = old_enough;

        let mut old = run(RunStatus::Queued, RunTrigger::Started);
        old.created_at = old_enough;
        assert_eq!(
            verdict(&old, Some(&card), now),
            Verdict::GiveUp {
                bound: GaveUpOn::QueuedWait,
                reason: WAITED_TOO_LONG,
            }
        );

        let resumed = claimed(old.clone());
        assert_eq!(
            verdict(&resumed, Some(&card), now),
            Verdict::Stands,
            "`started_at` is the FIRST claim, so age cannot bound a run that executed an hour ago"
        );
    }

    #[test]
    fn a_row_the_board_itself_parked_is_not_charged_for_the_wait() {
        let now = chrono::Utc::now();
        let mut card = issue(IssueStatus::InProgress);
        card.updated_at = now - chrono::Duration::minutes(1);

        let mut parked = run(RunStatus::Queued, RunTrigger::Started);
        parked.created_at = now - chrono::Duration::hours(MAX_QUEUED_WAIT_HOURS * 2);
        assert_eq!(
            verdict(&parked, Some(&card), now),
            Verdict::Stands,
            "the unblock is the moment the row became the board's to hand out again"
        );
    }

    #[test]
    fn the_cards_live_row_is_read_off_the_column_the_index_arbitrates() {
        let mut ended = run(RunStatus::Done, RunTrigger::Started);
        ended.settled_at = Some(chrono::Utc::now());
        let live = run(RunStatus::Queued, RunTrigger::Comment);

        assert!(unsettled(&[ended.clone()]).is_none());
        assert_eq!(
            unsettled(&[ended, live.clone()]).map(|row| &row.id),
            Some(&live.id)
        );

        let torn = run(RunStatus::Done, RunTrigger::Started);
        assert!(is_unsettled(&torn));
        assert_eq!(
            unsettled(std::slice::from_ref(&torn)).map(|row| &row.id),
            Some(&torn.id)
        );
    }

    #[test]
    fn a_released_hold_is_still_charged_the_wait_it_spent_held() {
        let now = chrono::Utc::now();
        let long_ago = now - chrono::Duration::hours(MAX_QUEUED_WAIT_HOURS * 2);
        let mut card = issue(IssueStatus::InProgress);
        card.updated_at = long_ago;

        let mut held = run(RunStatus::Held, RunTrigger::Started);
        held.created_at = long_ago;
        assert_eq!(
            verdict(&held, Some(&card), now),
            Verdict::Stands,
            "while it is held the wait is the budget's and the row is exempt"
        );

        let released = IssueRunRow {
            status: RunStatus::Queued,
            ..held
        };
        assert_eq!(
            verdict(&released, Some(&card), now),
            Verdict::GiveUp {
                bound: GaveUpOn::QueuedWait,
                reason: WAITED_TOO_LONG,
            },
            "and the release moves neither the row's clock nor the card's, so the same wait \
             is charged the moment the status changes"
        );
    }

    #[test]
    fn a_held_row_never_ages_out() {
        let now = chrono::Utc::now();
        let card = issue(IssueStatus::InProgress);
        let mut held = run(RunStatus::Held, RunTrigger::Started);
        held.created_at = now - chrono::Duration::days(7);
        assert_eq!(
            verdict(&held, Some(&card), now),
            Verdict::Stands,
            "a hold's age is the budget's doing; ageing it out drops work the board still owes"
        );
    }
}
