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

/// Build the ledger entry for a triggered run. Fails only if the issue has
/// no assignee, which [`triggers_run`] has already ruled out.
pub(crate) fn ledger_entry(issue: &IssueRow, trigger: RunTrigger) -> Option<NewIssueRun> {
    Some(NewIssueRun {
        id: IssueRunId::generate(),
        issue_id: issue.id.clone(),
        project_id: issue.project_id.clone(),
        number: issue.number,
        agent_id: issue.assignee.clone()?,
        trigger,
    })
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
}
