//! The two predicates a run has to satisfy, and the ledger entry it
//! becomes.
//!
//! They ask different questions and are deliberately not folded together.
//! [`triggers_run`] asks about an **edge**: did this particular write start
//! work? [`accepts_runs`] asks about a **card**: does this issue still take
//! work at all? Only the first needs a transition, and only some of the
//! doors into a run have one — a released hold, a boot re-drive, a retry
//! and a stage barrier all arrive with nothing but a row. So the card-level
//! rule cannot live here on the edge; it lives on the one path they all
//! share, [`crate::ProjectManager::enqueue`], which asks it once.

use baybo_model::{AgentProfileId, IssueRunId};
use baybo_store::project::{IssueRow, IssueStatus, NewIssueRun, RunTrigger};

/// What a write did to an issue, in the only terms [`triggers_run`] cares
/// about.
///
/// The assignee is carried whole rather than reduced to "somebody is on
/// it": handing live work to a *different* agent is an edge the predicate
/// has to see, and a bool cannot show it.
///
/// Cancellation is *not* carried, and that is the point: it is a fact about
/// the card rather than about the edge, so it is asked once at the enqueue
/// chokepoint (see [`accepts_runs`]) instead of once per predicate that
/// happens to have a transition in hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Transition {
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
///
/// Whether the card is one that still takes work at all is **not** asked
/// here: that is [`accepts_runs`], and
/// [`crate::ProjectManager::enqueue`] asks it for every door, this one
/// included.
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
///
/// A card the board has finished with does not, and "finished" is
/// [`crate::stages::is_finished`]'s single definition — Done, or cancelled.
/// The board acts on it everywhere else: `comment_delivery` refuses to wake
/// one, [`crate::stages`] counts it out of its stage, and
/// `reclaim_if_finished` has already taken its worktree back. So a run
/// started on one would cut a fresh checkout and put an agent to work on
/// abandoned work.
///
/// Asked of the card rather than of a write, because most of the doors into
/// a run have no write to look at: a hold released when the budget rolls
/// over, a row the boot sweep re-drives, a retry, and a parent woken by its
/// last step were all recorded — or last looked at — while the card was
/// still live. [`crate::ProjectManager::enqueue`] asks this once for all of
/// them, and the two sweeps that hand out rows recorded earlier ask it
/// again against the card as it is now.
///
/// Reviving a cancelled card, or dragging a Done one back, is what makes it
/// startable again.
pub(crate) fn accepts_runs(issue: &IssueRow) -> bool {
    !crate::stages::is_finished(issue)
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
        // anything either; the run outlives the drag. Both arms: the plain
        // drag, and a drag that also hands the card on.
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

    #[test]
    fn a_card_the_board_has_finished_with_takes_no_more_runs() {
        // Cancelled and Done are both "finished" everywhere else on the
        // board, and the worktree of either is already gone. Every door
        // into a run asks this, not just the ones that carry a transition.
        let mut cancelled = issue(IssueStatus::InProgress);
        cancelled.cancelled_at = Some(chrono::Utc::now());
        assert!(!accepts_runs(&cancelled), "cancelled");
        assert!(!accepts_runs(&issue(IssueStatus::Done)), "done");
        // Cancelled outranks the column it is parked in.
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
