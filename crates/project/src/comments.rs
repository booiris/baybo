//! What happens when somebody says something on an issue.
//!
//! Pure, like [`crate::runs::triggers_run`] and
//! [`crate::timeline::diff_events`]: this is the product rule the composer
//! promises and the manager carries out, and both have to read it the same
//! way.

use baybo_store::project::{IssueRow, IssueStatus, RunStatus};

/// Where a comment goes besides the timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentDelivery {
    /// Nowhere. Nobody is on the issue, or it is parked in a column where
    /// nobody is working — the comment is history, and the composer says
    /// so before it is sent.
    RecordOnly,
    /// Start a run: somebody is assigned, the work is live, and nothing is
    /// currently reading.
    Wake,
    /// A run is queued but has not started. It assembles its brief when it
    /// starts, so it will read this — a second run would be two agents on
    /// one card.
    WaitsForQueuedRun,
    /// A run is executing and is already past assembling its brief. The
    /// comment is picked up by a follow-up run enqueued when this one
    /// settles, so it is never lost and never interrupts.
    AfterCurrentRun,
}

/// The columns where an assignee is understood to be working. Backlog is
/// not yet and Done is no longer — a comment there is a note for later,
/// not an instruction.
fn is_live_work(status: IssueStatus) -> bool {
    matches!(
        status,
        IssueStatus::Todo | IssueStatus::InProgress | IssueStatus::Review
    )
}

/// Decide a comment's delivery.
///
/// `live_run` is the issue's unsettled run, if it has one — at most one by
/// construction (the per-issue partial unique index).
pub fn comment_delivery(issue: &IssueRow, live_run: Option<RunStatus>) -> CommentDelivery {
    if issue.assignee.is_none() || issue.cancelled_at.is_some() || !is_live_work(issue.status) {
        return CommentDelivery::RecordOnly;
    }
    match live_run {
        Some(RunStatus::Queued) => CommentDelivery::WaitsForQueuedRun,
        Some(RunStatus::Running) => CommentDelivery::AfterCurrentRun,
        // A settled run is history; the issue is idle again.
        _ => CommentDelivery::Wake,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::{AgentProfileId, IssueId, ProjectId};
    use baybo_store::project::IssuePriority;

    fn issue(status: IssueStatus, assigned: bool) -> IssueRow {
        let now = chrono::Utc::now();
        IssueRow {
            id: IssueId::generate(),
            project_id: ProjectId::parse("proj-a".to_owned()).expect("id"),
            number: 1,
            title: "Wire it".into(),
            description: String::new(),
            status,
            priority: IssuePriority::None,
            assignee: assigned.then(|| AgentProfileId::parse("dev-1".to_owned()).expect("agent")),
            position: 0,
            blocked_reason: None,
            branch: None,
            cancelled_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn nobody_assigned_means_nobody_to_tell() {
        assert_eq!(
            comment_delivery(&issue(IssueStatus::InProgress, false), None),
            CommentDelivery::RecordOnly
        );
    }

    #[test]
    fn parked_work_records_even_with_an_assignee() {
        // Backlog is not yet and Done is no longer. Waking an agent to read
        // a note on finished work is the board acting on its own.
        for status in [IssueStatus::Backlog, IssueStatus::Done] {
            assert_eq!(
                comment_delivery(&issue(status, true), None),
                CommentDelivery::RecordOnly,
                "{status:?}"
            );
        }
    }

    #[test]
    fn a_cancelled_issue_never_wakes_anybody() {
        let mut cancelled = issue(IssueStatus::InProgress, true);
        cancelled.cancelled_at = Some(chrono::Utc::now());
        assert_eq!(
            comment_delivery(&cancelled, None),
            CommentDelivery::RecordOnly
        );
    }

    #[test]
    fn live_work_with_nobody_reading_starts_a_run() {
        for status in [
            IssueStatus::Todo,
            IssueStatus::InProgress,
            IssueStatus::Review,
        ] {
            assert_eq!(
                comment_delivery(&issue(status, true), None),
                CommentDelivery::Wake,
                "{status:?}"
            );
            // A settled run leaves the issue idle again, so the next
            // comment starts a fresh one rather than being swallowed.
            assert_eq!(
                comment_delivery(&issue(status, true), Some(RunStatus::Done)),
                CommentDelivery::Wake
            );
        }
    }

    #[test]
    fn a_run_that_has_not_started_yet_reads_the_comment_itself() {
        // The queued run assembles its brief when it starts, so it picks
        // this up. Enqueuing a second would be two agents on one card —
        // which the dedupe index would refuse anyway, losing the comment.
        assert_eq!(
            comment_delivery(
                &issue(IssueStatus::InProgress, true),
                Some(RunStatus::Queued)
            ),
            CommentDelivery::WaitsForQueuedRun
        );
    }

    #[test]
    fn a_running_run_is_already_past_its_brief_so_the_next_one_takes_it() {
        assert_eq!(
            comment_delivery(
                &issue(IssueStatus::InProgress, true),
                Some(RunStatus::Running)
            ),
            CommentDelivery::AfterCurrentRun
        );
    }
}
