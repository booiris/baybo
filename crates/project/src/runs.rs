//! The trigger predicate and the enqueue path.
//!
//! One function decides whether a run happens, and every write path — a
//! drag, a REST move, an assignment, a future agent tool — goes through
//! it. That is the whole point: a second predicate somewhere else is how
//! a board ends up with two different answers to "does this start now?".

use baybo_model::IssueRunId;
use baybo_store::project::{IssueRow, IssueStatus, NewIssueRun, RunTrigger};

/// What a write did to an issue, in the only terms the predicate cares
/// about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    pub before_status: IssueStatus,
    pub before_assigned: bool,
    pub after_status: IssueStatus,
    pub after_assigned: bool,
}

/// Whether this transition starts work, and why.
///
/// Two edges lead into a run, and they are deliberately both here rather
/// than one being special-cased at a call site:
///
/// * the issue **enters** In Progress with somebody on it — a drag, a REST
///   move, an agent's own status change;
/// * an agent is **put on** an issue already sitting in In Progress —
///   without this, assigning a card that is already in the column leaves
///   the board claiming work is under way that nobody started.
///
/// Everything else is not a trigger: staying in the column, leaving it,
/// re-ordering, editing prose. In particular, dragging *out* of In
/// Progress does not stop anything — the run outlives the column, and
/// cancelling it is a separate, explicit act.
pub fn triggers_run(t: Transition) -> Option<RunTrigger> {
    if t.after_status != IssueStatus::InProgress || !t.after_assigned {
        return None;
    }
    if t.before_status != IssueStatus::InProgress {
        return Some(RunTrigger::Started);
    }
    if !t.before_assigned {
        return Some(RunTrigger::Assigned);
    }
    None
}

impl Transition {
    /// The transition between two states of the same issue.
    pub fn between(before: &IssueRow, after: &IssueRow) -> Self {
        Self {
            before_status: before.status,
            before_assigned: before.assignee.is_some(),
            after_status: after.status,
            after_assigned: after.assignee.is_some(),
        }
    }

    /// A newly created issue: it came from nowhere, so the "before" is a
    /// state that can never itself be a trigger.
    pub fn created(after: &IssueRow) -> Self {
        Self {
            before_status: IssueStatus::Backlog,
            before_assigned: false,
            after_status: after.status,
            after_assigned: after.assignee.is_some(),
        }
    }
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

    fn t(
        before_status: IssueStatus,
        before_assigned: bool,
        after_status: IssueStatus,
        after_assigned: bool,
    ) -> Transition {
        Transition {
            before_status,
            before_assigned,
            after_status,
            after_assigned,
        }
    }

    #[test]
    fn entering_in_progress_with_somebody_on_it_starts_work() {
        assert_eq!(
            triggers_run(t(IssueStatus::Todo, true, IssueStatus::InProgress, true)),
            Some(RunTrigger::Started)
        );
        // Wherever it came from.
        for from in [IssueStatus::Backlog, IssueStatus::Review, IssueStatus::Done] {
            assert_eq!(
                triggers_run(t(from, true, IssueStatus::InProgress, true)),
                Some(RunTrigger::Started)
            );
        }
    }

    #[test]
    fn assigning_a_card_already_in_the_column_starts_work_too() {
        // Without this edge the board would show work in flight that
        // nobody ever started.
        assert_eq!(
            triggers_run(t(
                IssueStatus::InProgress,
                false,
                IssueStatus::InProgress,
                true
            )),
            Some(RunTrigger::Assigned)
        );
    }

    #[test]
    fn nothing_else_starts_work() {
        // Already running and still assigned: an edit is not a restart.
        assert!(
            triggers_run(t(
                IssueStatus::InProgress,
                true,
                IssueStatus::InProgress,
                true
            ))
            .is_none()
        );
        // Leaving the column never starts anything — and never stops
        // anything either; the run outlives the drag.
        assert!(
            triggers_run(t(IssueStatus::InProgress, true, IssueStatus::Review, true)).is_none()
        );
        // Unassigned work cannot start, and the manager refuses this state
        // anyway.
        assert!(
            triggers_run(t(IssueStatus::Todo, false, IssueStatus::InProgress, false)).is_none()
        );
        // Moving between other columns is just moving.
        assert!(triggers_run(t(IssueStatus::Backlog, true, IssueStatus::Todo, true)).is_none());
    }
}
