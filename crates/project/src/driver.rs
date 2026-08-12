//! What the board starts without being told to.
//!
//! Todo is "ready, waiting for capacity", and this is the thing that
//! notices the capacity. Everything here is a rule over rows the caller
//! already read — the manager owns the reads, the writes, and the order
//! they happen in; this module owns *which cards*, and nothing else.
//!
//! Two rules, and they are deliberately different shapes:
//!
//! - [`slate`] is **level-triggered**. It answers "given the board as it
//!   is right now, what should be running?", so running it twice on an
//!   unchanged board promotes nothing the second time and a missed call
//!   costs a delay rather than a card.
//! - [`awaiting_triage`] is level-triggered too, but waking the lead does
//!   not change what it looks at — an unassigned card the lead decided to
//!   leave alone is still an unassigned card. [`already_triaged`] is what
//!   stops that from being a loop that bills for the same question every
//!   time a run ends anywhere on the board.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use baybo_store::project::{IssuePriority, IssueRow, IssueRunRow, IssueStatus, RunTrigger};

/// The cards that already have a run recorded against them, by number.
///
/// A card holds one run slot, so one that is already spoken for cannot be
/// given another — and promoting it anyway would move it into In Progress
/// and then fail to start anything, which is the one state that column must
/// never be in. A comment on a Todo card wakes its assignee where it
/// stands, so this is not a rare shape.
pub(crate) fn busy(runs: impl IntoIterator<Item = i64>) -> BTreeSet<i64> {
    runs.into_iter().collect()
}

/// Whether this card is genuinely waiting on the board — everything the
/// driver needs to be true *before* it looks at who is on it.
///
/// Stricter than [`accepts_runs`](crate::runs::accepts_runs), which asks
/// only whether a card is still live: a person dragging a blocked card into
/// In Progress is overriding the block on purpose, and the board doing the
/// same thing on its own would be overriding it on nobody's authority.
///
/// The two things the driver can do about a waiting card — start it, or ask
/// who should — split on staffing and on nothing else, so every other gate
/// belongs here rather than once per branch.
fn is_waiting(issue: &IssueRow, busy: &BTreeSet<i64>) -> bool {
    issue.status == IssueStatus::Todo
        && issue.cancelled_at.is_none()
        && issue.blocked_reason.is_none()
        && !busy.contains(&issue.number)
}

/// A waiting card somebody is on: the board can start it.
fn is_promotable(issue: &IssueRow, busy: &BTreeSet<i64>) -> bool {
    is_waiting(issue, busy) && issue.assignee.is_some()
}

/// A waiting card nobody is on: the board can only ask who should be.
///
/// Deliberately not `!is_promotable`, which would be true of every card on
/// the board — cancelled, blocked, in another column, already running — and
/// would send all of them to the lead as "unstaffed work".
fn needs_staffing(issue: &IssueRow, busy: &BTreeSet<i64>) -> bool {
    is_waiting(issue, busy) && issue.assignee.is_none()
}

/// Which of two ready cards goes first: the more urgent, then the one the
/// operator put higher in the column.
///
/// The same order [`IssueList`](crate::tools::IssueList) already reads a
/// column in, and that is the point — "what is next in Todo" has one
/// answer, whether an agent asks for it or the board acts on it.
fn promotion_order(a: &IssueRow, b: &IssueRow) -> Ordering {
    let rank = |p: IssuePriority| IssuePriority::ALL.iter().position(|slot| *slot == p);
    rank(a.priority)
        .cmp(&rank(b.priority))
        .then(a.position.cmp(&b.position))
        // Positions are dense within a column, so this only decides between
        // two rows mid-reorder. It still has to be *something*: a sort that
        // can return Equal for distinct cards makes the slate depend on the
        // order the store happened to return.
        .then(a.number.cmp(&b.number))
}

/// The cards to move into In Progress, best first, at most `slots` of them.
pub(crate) fn slate(issues: &[IssueRow], busy: &BTreeSet<i64>, slots: usize) -> Vec<IssueRow> {
    if slots == 0 {
        return Vec::new();
    }
    let mut ready: Vec<IssueRow> = issues
        .iter()
        .filter(|i| is_promotable(i, busy))
        .cloned()
        .collect();
    ready.sort_by(promotion_order);
    ready.truncate(slots);
    ready
}

/// The cards sitting in Todo with nobody on them, in the order the lead
/// should be asked about them.
pub(crate) fn awaiting_triage(issues: &[IssueRow], busy: &BTreeSet<i64>) -> Vec<IssueRow> {
    let mut unstaffed: Vec<IssueRow> = issues
        .iter()
        .filter(|i| needs_staffing(i, busy))
        .cloned()
        .collect();
    unstaffed.sort_by(promotion_order);
    unstaffed
}

/// Whether the lead has already been asked about this card **as it stands**.
///
/// The comparison is against the card's own `updated_at` rather than a flag,
/// because the honest question is "has anything changed since the lead
/// looked?" — and a lead that read the card and decided to leave it
/// unstaffed changed nothing, which is precisely the case that must not ask
/// again. Editing the card, or moving it, makes it a new question.
pub(crate) fn already_triaged(issue: &IssueRow, runs: &[IssueRunRow]) -> bool {
    runs.iter()
        .any(|run| run.trigger == RunTrigger::Triage && run.created_at >= issue.updated_at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::{AgentProfileId, IssueId, IssueRunId, ProjectId};
    use baybo_store::project::RunStatus;
    use chrono::{Duration, Utc};

    fn issue(number: i64, status: IssueStatus) -> IssueRow {
        let now = Utc::now();
        IssueRow {
            id: IssueId::generate(),
            project_id: ProjectId::parse("proj-a".to_owned()).expect("id"),
            number,
            title: format!("card {number}"),
            description: String::new(),
            attachments: Vec::new(),
            status,
            priority: IssuePriority::None,
            assignee: Some(AgentProfileId::parse("dev-1".to_owned()).expect("agent")),
            position: number,
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

    fn ready(number: i64) -> IssueRow {
        issue(number, IssueStatus::Todo)
    }

    fn numbers(slate: &[IssueRow]) -> Vec<i64> {
        slate.iter().map(|i| i.number).collect()
    }

    /// A board with nothing recorded against any card.
    fn idle() -> BTreeSet<i64> {
        BTreeSet::new()
    }

    /// One reason to leave a card alone: why, and how to put a card in that
    /// state.
    type Gate = (&'static str, Box<dyn Fn(&mut IssueRow)>);

    /// Every reason **on the row itself** for the driver to leave a card
    /// alone, other than staffing. The fifth gate, `busy`, is a separate
    /// input rather than a field, so it has its own test below — asserting
    /// the same two things.
    ///
    /// Driven as a table because these are the clauses [`is_waiting`] holds
    /// on behalf of both branches: a gate that stops a promotion has to stop
    /// a triage too, and one added to only one of them is a rule the other
    /// quietly ignores. A new gate is a new row here, asserted on both sides
    /// for free.
    fn gates() -> Vec<Gate> {
        let mut rows: Vec<Gate> = vec![
            (
                "a block is a person saying stop, and the board does not overrule it",
                Box::new(|i: &mut IssueRow| {
                    i.blocked_reason = Some("waiting on the operator".into())
                }),
            ),
            (
                "a cancelled card is not live work",
                Box::new(|i: &mut IssueRow| i.cancelled_at = Some(Utc::now())),
            ),
        ];
        for status in [
            IssueStatus::Backlog,
            IssueStatus::InProgress,
            IssueStatus::Review,
            IssueStatus::Done,
        ] {
            rows.push((
                "only Todo is a queue the board pulls from",
                Box::new(move |i: &mut IssueRow| i.status = status),
            ));
        }
        rows
    }

    #[test]
    fn staffing_is_the_only_thing_the_two_branches_disagree_about() {
        assert!(
            is_promotable(&ready(1), &idle()),
            "the control: a staffed card in Todo with nothing against it is startable"
        );

        for (why, break_it) in gates() {
            let mut staffed = ready(1);
            break_it(&mut staffed);
            let mut unstaffed = staffed.clone();
            unstaffed.assignee = None;

            assert!(!is_promotable(&staffed, &idle()), "promoted anyway: {why}");
            assert!(
                awaiting_triage(&[unstaffed], &idle()).is_empty(),
                "sent to the lead as unstaffed work anyway: {why}"
            );
        }
    }

    #[test]
    fn the_board_never_starts_work_nobody_is_on() {
        let mut unstaffed = ready(1);
        unstaffed.assignee = None;
        assert!(
            !is_promotable(&unstaffed, &idle()),
            "In Progress needs an assignee, so an unassigned card cannot be promoted into it"
        );
        assert_eq!(
            numbers(&awaiting_triage(&[unstaffed], &idle())),
            vec![1],
            "it is the lead's question instead"
        );
    }

    #[test]
    fn urgent_jumps_the_queue_and_position_settles_the_rest() {
        let mut low = ready(1);
        low.priority = IssuePriority::Low;
        low.position = 0;
        let mut urgent = ready(2);
        urgent.priority = IssuePriority::Urgent;
        urgent.position = 1;
        let mut also_low = ready(3);
        also_low.priority = IssuePriority::Low;
        also_low.position = 2;

        assert_eq!(
            numbers(&slate(&[low, urgent, also_low], &idle(), 3)),
            vec![2, 1, 3],
            "priority first, then the order the operator put the column in"
        );
    }

    #[test]
    fn the_slate_is_capped_by_the_slots_it_was_given() {
        let column: Vec<IssueRow> = (1..=5).map(ready).collect();
        assert_eq!(numbers(&slate(&column, &idle(), 2)), vec![1, 2]);
        assert!(
            slate(&column, &idle(), 0).is_empty(),
            "a full board starts nothing, and a ceiling of zero is a full board"
        );
        assert_eq!(
            numbers(&slate(&column, &idle(), 99)),
            vec![1, 2, 3, 4, 5],
            "more room than work is not an error"
        );
    }

    #[test]
    fn a_card_that_already_has_a_run_is_left_where_it_is() {
        let ready_and_running = ready(1);
        let mut unstaffed_and_running = ready(2);
        unstaffed_and_running.assignee = None;
        let busy = busy([1, 2]);

        assert!(
            slate(&[ready_and_running], &busy, 3).is_empty(),
            "promoting it would move it into In Progress and then start nothing, \
             because a card holds one run slot"
        );
        assert!(
            awaiting_triage(&[unstaffed_and_running], &busy).is_empty(),
            "and a card the lead is already looking at is not a fresh question"
        );
    }

    fn triage_run(issue: &IssueRow, at: chrono::DateTime<Utc>) -> IssueRunRow {
        IssueRunRow {
            id: IssueRunId::generate(),
            issue_id: issue.id.clone(),
            project_id: issue.project_id.clone(),
            number: issue.number,
            agent_id: AgentProfileId::parse("lead".to_owned()).expect("agent"),
            session_id: None,
            trigger: RunTrigger::Triage,
            status: RunStatus::Done,
            attempt: 1,
            error: None,
            created_at: at,
            started_at: None,
            settled_at: None,
        }
    }

    #[test]
    fn the_lead_is_asked_once_per_state_of_a_card() {
        let mut card = ready(1);
        card.assignee = None;
        assert!(
            !already_triaged(&card, &[]),
            "a card nobody has looked at is a fresh question"
        );

        let asked = triage_run(&card, card.updated_at + Duration::seconds(1));
        assert!(
            already_triaged(&card, std::slice::from_ref(&asked)),
            "a lead that read the card and left it alone must not be asked again"
        );

        card.updated_at = asked.created_at + Duration::seconds(1);
        assert!(
            !already_triaged(&card, std::slice::from_ref(&asked)),
            "but editing the card makes it a question the lead has not answered"
        );

        let other = IssueRunRow {
            trigger: RunTrigger::Comment,
            ..triage_run(&card, card.updated_at + Duration::seconds(1))
        };
        assert!(
            !already_triaged(&card, &[asked, other]),
            "and a run that was not a triage is not an answer to one"
        );
    }
}
