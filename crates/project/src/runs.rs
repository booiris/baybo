//! The trigger predicate and the enqueue path.
//!
//! One function decides whether a run happens, and every write path — a
//! drag, a REST move, an assignment, a future agent tool — goes through
//! it. That is the whole point: a second predicate somewhere else is how
//! a board ends up with two different answers to "does this start now?".

use baybo_model::{AgentProfileId, IssueRunId};
use baybo_store::project::{IssueRow, IssueStatus, NewIssueRun, RunTrigger};

/// What a write did to an issue, in the only terms the predicate cares
/// about.
///
/// The assignee is carried whole rather than reduced to "somebody is on
/// it": handing live work to a *different* agent is an edge the predicate
/// has to see, and a bool cannot show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub before_status: IssueStatus,
    pub before_assignee: Option<AgentProfileId>,
    pub after_status: IssueStatus,
    pub after_assignee: Option<AgentProfileId>,
}

/// Whether this transition starts work, and why.
///
/// Two edges lead into a run, and they are deliberately both here rather
/// than one being special-cased at a call site:
///
/// * the issue **enters** In Progress with somebody on it — a drag, a REST
///   move, an agent's own status change;
/// * an agent is **put on** an issue already sitting in In Progress,
///   including in place of a different one — without this, assigning a card
///   that is already in the column leaves the board claiming work is under
///   way that nobody started.
///
/// Everything else is not a trigger: staying in the column, leaving it,
/// re-ordering, editing prose. In particular, dragging *out* of In
/// Progress does not stop anything — the run outlives the column, and
/// cancelling it is a separate, explicit act.
pub fn triggers_run(t: Transition) -> Option<RunTrigger> {
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

impl Transition {
    /// The transition between two states of the same issue.
    pub fn between(before: &IssueRow, after: &IssueRow) -> Self {
        Self {
            before_status: before.status,
            before_assignee: before.assignee.clone(),
            after_status: after.status,
            after_assignee: after.assignee.clone(),
        }
    }

    /// A newly created issue: it came from nowhere, so the "before" is a
    /// state that can never itself be a trigger.
    pub fn created(after: &IssueRow) -> Self {
        Self {
            before_status: IssueStatus::Backlog,
            before_assignee: None,
            after_status: after.status,
            after_assignee: after.assignee.clone(),
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
        // Wherever it came from.
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
        // Without this edge the board would show work in flight that
        // nobody ever started.
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
        // A handover on live work is still a start: the new agent has done
        // nothing yet, and a card showing @dev-2 on work only @dev-1 ever
        // touched is the board lying about who is doing it.
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
        // Already running and still the same agent: an edit is not a
        // restart.
        assert!(
            triggers_run(t(
                IssueStatus::InProgress,
                agent("dev-1"),
                IssueStatus::InProgress,
                agent("dev-1")
            ))
            .is_none()
        );
        // Leaving the column never starts anything — and never stops
        // anything either; the run outlives the drag.
        assert!(
            triggers_run(t(
                IssueStatus::InProgress,
                agent("dev-1"),
                IssueStatus::Review,
                agent("dev-2")
            ))
            .is_none()
        );
        // Handing finished work to somebody else does not reopen it.
        assert!(
            triggers_run(t(
                IssueStatus::Done,
                agent("dev-1"),
                IssueStatus::Done,
                agent("dev-2")
            ))
            .is_none()
        );
        // Unassigned work cannot start, and the manager refuses this state
        // anyway.
        assert!(triggers_run(t(IssueStatus::Todo, None, IssueStatus::InProgress, None)).is_none());
        // Moving between other columns is just moving.
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
}
