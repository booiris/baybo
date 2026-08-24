//! Sub-issues and the barriers between them.

use baybo_store::project::{IssueRow, IssueStatus};

/// Whether the board is done with this issue.
pub(crate) fn is_finished(issue: &IssueRow) -> bool {
    issue.status == IssueStatus::Done || issue.cancelled_at.is_some()
}

fn is_pending(child: &IssueRow) -> bool {
    child.cancelled_at.is_none() && child.status != IssueStatus::Done
}

/// Whether `stage` has just emptied, given the parent's children and the
/// child that reached a terminal state.
pub(crate) fn stage_complete(children: &[IssueRow], stage: i64) -> bool {
    let mut seen = false;
    for child in children.iter().filter(|c| c.stage == stage) {
        seen = true;
        if is_pending(child) {
            return false;
        }
    }
    seen
}

/// Whether finishing a child in `stage` opens a barrier: that stage has
/// emptied **and** nothing earlier is still open.
pub(crate) fn barrier_opens(children: &[IssueRow], stage: i64) -> bool {
    stage_complete(children, stage) && !children.iter().any(|c| is_pending(c) && c.stage < stage)
}

/// `(done, total)` for a parent's card, counting only work that is still
/// meant to happen.
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
pub(crate) fn open_stages(children: &[IssueRow]) -> Vec<i64> {
    let mut stages: Vec<i64> = children
        .iter()
        .filter(|c| is_pending(c))
        .map(|c| c.stage)
        .collect();
    stages.sort_unstable();
    stages.dedup();
    stages
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
            attachments: Vec::new(),
            status,
            priority: IssuePriority::None,
            assignee: None,
            position: 0,
            pinned: false,
            blocked_reason: None,
            branch: None,
            parent_issue_id: Some(IssueId::generate()),
            stage,
            source_key: None,
            filed_from: None,
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
        let children = vec![
            child(0, IssueStatus::Done, false),
            child(0, IssueStatus::Todo, true),
        ];
        assert!(stage_complete(&children, 0));
    }

    #[test]
    fn an_empty_stage_has_not_completed() {
        assert!(!stage_complete(&[], 0));
        assert!(!stage_complete(&[child(1, IssueStatus::Done, false)], 0));
    }

    #[test]
    fn a_later_stage_emptying_first_is_not_a_barrier() {
        let children = vec![
            child(0, IssueStatus::InProgress, false),
            child(2, IssueStatus::Done, false),
        ];
        assert!(stage_complete(&children, 2));
        assert!(!barrier_opens(&children, 2));

        let children = vec![
            child(0, IssueStatus::Done, false),
            child(2, IssueStatus::Done, false),
        ];
        assert!(
            barrier_opens(&children, 0),
            "stage 0 opened on its own turn"
        );
    }

    #[test]
    fn the_last_stage_emptying_still_opens_the_barrier() {
        assert!(barrier_opens(&[child(0, IssueStatus::Done, false)], 0));
    }

    #[test]
    fn an_unfinished_stage_never_opens_its_own_barrier() {
        let children = vec![
            child(0, IssueStatus::Done, false),
            child(0, IssueStatus::Todo, false),
        ];
        assert!(!barrier_opens(&children, 0));
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
    fn finished_means_done_or_called_off() {
        assert!(is_finished(&child(0, IssueStatus::Done, false)));
        assert!(is_finished(&child(0, IssueStatus::Todo, true)));
        assert!(!is_finished(&child(0, IssueStatus::InProgress, false)));
    }
}
