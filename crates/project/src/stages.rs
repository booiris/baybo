//! Sub-issues and the barriers between them.
//!
//! Pure, like the other rule modules here: the barrier decides one thing —
//! did this stage just finish — and both the wake and the card's progress
//! ring read it from the same place.

use baybo_store::project::{IssueRow, IssueStatus};

/// Whether a child counts as finished for a barrier.
///
/// Cancelled counts. A stage waiting on work somebody decided not to do
/// would never open, and "cancel the step you are not doing" is exactly how
/// an operator unblocks one.
fn is_finished(child: &IssueRow) -> bool {
    child.status == IssueStatus::Done || child.cancelled_at.is_some()
}

/// A child that still has to happen for its stage to open the next one.
///
/// Cancelled children are not pending, but they are also not *progress* —
/// see [`progress`], which counts them out of both numerator and
/// denominator so a ring never reads `3/5` on a card whose last two steps
/// were called off.
fn is_pending(child: &IssueRow) -> bool {
    child.cancelled_at.is_none() && child.status != IssueStatus::Done
}

/// Whether `stage` has just emptied, given the parent's children and the
/// child that reached a terminal state.
///
/// Returns `false` when the stage has no children at all: nothing finished,
/// so there is nothing to announce. That is not a hypothetical — detaching
/// or cancelling the last child of a stage would otherwise read as a
/// completion.
pub fn stage_complete(children: &[IssueRow], stage: i64) -> bool {
    let mut seen = false;
    for child in children.iter().filter(|c| c.stage == stage) {
        seen = true;
        if is_pending(child) {
            return false;
        }
    }
    seen
}

/// `(done, total)` for a parent's card, counting only work that is still
/// meant to happen.
///
/// Cancelled children leave both counts, so a parent whose last two steps
/// were called off reads `3/3` rather than `3/5` — the ring means "how much
/// of the remaining work is done", which is the question somebody looking
/// at a card is asking.
pub fn progress(children: &[IssueRow]) -> (usize, usize) {
    let live: Vec<&IssueRow> = children
        .iter()
        .filter(|c| c.cancelled_at.is_none())
        .collect();
    let done = live
        .iter()
        .filter(|c| c.status == IssueStatus::Done)
        .count();
    (done, live.len())
}

/// The stages that still have unfinished work, lowest first — what the
/// parent's assignee is being woken to drive.
pub fn open_stages(children: &[IssueRow]) -> Vec<i64> {
    let mut stages: Vec<i64> = children
        .iter()
        .filter(|c| is_pending(c))
        .map(|c| c.stage)
        .collect();
    stages.sort_unstable();
    stages.dedup();
    stages
}

/// Whether every stage is finished — the parent's own work can be closed.
pub fn all_finished(children: &[IssueRow]) -> bool {
    !children.is_empty() && children.iter().all(is_finished)
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::{IssueId, ProjectId};
    use baybo_store::project::IssuePriority;

    fn child(stage: i64, status: IssueStatus, cancelled: bool) -> IssueRow {
        let now = chrono::Utc::now();
        IssueRow {
            id: IssueId::generate(),
            project_id: ProjectId::parse("p").expect("id"),
            number: 1,
            title: "step".into(),
            description: String::new(),
            status,
            priority: IssuePriority::None,
            assignee: None,
            position: 0,
            blocked_reason: None,
            branch: None,
            parent_issue_id: Some(IssueId::generate()),
            stage,
            source_key: None,
            cancelled_at: cancelled.then_some(now),
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn a_stage_is_complete_only_when_none_of_it_is_pending() {
        let children = vec![
            child(0, IssueStatus::Done, false),
            child(0, IssueStatus::InProgress, false),
            child(1, IssueStatus::Backlog, false),
        ];
        assert!(!stage_complete(&children, 0));
        assert!(!stage_complete(&children, 1));

        let children = vec![
            child(0, IssueStatus::Done, false),
            child(0, IssueStatus::Done, false),
            child(1, IssueStatus::Backlog, false),
        ];
        assert!(stage_complete(&children, 0), "stage 0 opened stage 1");
        assert!(!stage_complete(&children, 1));
    }

    #[test]
    fn a_cancelled_step_does_not_hold_its_stage_open() {
        // Otherwise a stage waiting on work somebody decided not to do
        // would never open, and cancelling is how an operator unblocks one.
        let children = vec![
            child(0, IssueStatus::Done, false),
            child(0, IssueStatus::Todo, true),
        ];
        assert!(stage_complete(&children, 0));
    }

    #[test]
    fn an_empty_stage_has_not_completed() {
        // Nothing finished, so there is nothing to announce. Detaching the
        // last child of a stage must not read as that stage being done.
        assert!(!stage_complete(&[], 0));
        assert!(!stage_complete(&[child(1, IssueStatus::Done, false)], 0));
    }

    #[test]
    fn progress_counts_only_work_still_meant_to_happen() {
        let children = vec![
            child(0, IssueStatus::Done, false),
            child(0, IssueStatus::Done, false),
            child(0, IssueStatus::Done, false),
            child(1, IssueStatus::Todo, true),
            child(1, IssueStatus::Todo, true),
        ];
        // Not 3/5: two steps were called off, so the card is finished.
        assert_eq!(progress(&children), (3, 3));
        assert_eq!(progress(&[]), (0, 0));
    }

    #[test]
    fn open_stages_are_the_ones_with_work_left_in_them() {
        let children = vec![
            child(0, IssueStatus::Done, false),
            child(1, IssueStatus::Todo, false),
            child(1, IssueStatus::Review, false),
            child(2, IssueStatus::Backlog, false),
            child(2, IssueStatus::Todo, true),
        ];
        assert_eq!(open_stages(&children), vec![1, 2], "deduped and sorted");
        assert!(open_stages(&[child(0, IssueStatus::Done, false)]).is_empty());
    }

    #[test]
    fn all_finished_needs_children_to_be_about() {
        // An issue with no sub-issues has not "finished all its stages" —
        // it has none, and treating that as completion would close every
        // ordinary card the moment anything looked at it.
        assert!(!all_finished(&[]));
        assert!(all_finished(&[
            child(0, IssueStatus::Done, false),
            child(1, IssueStatus::Todo, true),
        ]));
        assert!(!all_finished(&[
            child(0, IssueStatus::Done, false),
            child(1, IssueStatus::Todo, false),
        ]));
    }
}
