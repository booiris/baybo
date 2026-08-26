//! What happens when somebody says something on an issue.

use baybo_store::project::{IssueActor, IssueEventBody, IssueEventRow, IssueRow, RunStatus};

/// Where a comment goes besides the timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentDelivery {
    /// Nowhere. Nobody is on the issue, or it is parked in a column where
    /// nobody is working — the comment is history, and the composer says
    /// so before it is sent.
    RecordOnly,
    /// Recorded while a block prevents delivery or assignment.
    ParkedByABlock,
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

/// Decide a comment's delivery.
pub(crate) fn comment_delivery(issue: &IssueRow, live_run: Option<RunStatus>) -> CommentDelivery {
    if issue.assignee.is_none()
        || !crate::runs::accepts_runs(issue)
        || !crate::driver::is_live_work(issue.status)
    {
        return CommentDelivery::RecordOnly;
    }
    if !crate::driver::board_may_start(issue) {
        return CommentDelivery::ParkedByABlock;
    }
    match live_run {
        // A held run has not assembled its brief either — it is waiting on
        // the board's budget, not on work — so the comment lands in it.
        Some(RunStatus::Held | RunStatus::Queued) => CommentDelivery::WaitsForQueuedRun,
        Some(RunStatus::Running) => CommentDelivery::AfterCurrentRun,
        // A settled run is history; the issue is idle again.
        _ => CommentDelivery::Wake,
    }
}

/// Whether someone other than the next runner commented in this window.
pub(crate) fn somebody_asked_for_more<'a>(
    window: impl IntoIterator<Item = &'a IssueEventRow>,
    next_runner: &IssueActor,
) -> bool {
    window
        .into_iter()
        .any(|e| matches!(e.body, IssueEventBody::Comment { .. }) && &e.actor != next_runner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::{AgentProfileId, IssueId, ProjectId};
    use baybo_store::project::{IssuePriority, IssueStatus};

    fn issue(status: IssueStatus, assigned: bool) -> IssueRow {
        let now = chrono::Utc::now();
        IssueRow {
            id: IssueId::generate(),
            project_id: ProjectId::parse("proj-a".to_owned()).expect("id"),
            number: 1,
            title: "Wire it".into(),
            description: String::new(),
            attachments: Vec::new(),
            status,
            priority: IssuePriority::None,
            assignee: assigned.then(|| AgentProfileId::parse("dev-1".to_owned()).expect("agent")),
            position: 0,
            pinned: false,
            blocked_reason: None,
            branch: None,
            parent_issue_id: None,
            stage: 0,
            source_key: None,
            filed_from: None,
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
            assert_eq!(
                comment_delivery(&issue(status, true), Some(RunStatus::Done)),
                CommentDelivery::Wake
            );
        }
    }

    #[test]
    fn a_run_that_has_not_started_yet_reads_the_comment_itself() {
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

    #[test]
    fn nothing_is_woken_on_a_card_a_block_has_stopped() {
        for live in [
            None,
            Some(RunStatus::Queued),
            Some(RunStatus::Held),
            Some(RunStatus::Running),
            Some(RunStatus::Done),
        ] {
            let mut paused = issue(IssueStatus::InProgress, true);
            paused.blocked_reason = Some("which of the two goals wins?".into());
            assert_eq!(
                comment_delivery(&paused, live),
                CommentDelivery::ParkedByABlock,
                "{live:?}"
            );
        }

        let mut unassigned = issue(IssueStatus::InProgress, false);
        unassigned.blocked_reason = Some("waiting on the operator".into());
        assert_eq!(
            comment_delivery(&unassigned, None),
            CommentDelivery::RecordOnly,
            "a card nobody is on is answered by who is missing, not by the block"
        );
    }
}
