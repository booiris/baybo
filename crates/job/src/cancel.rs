//! Cancellation reasons for `JobStatus::Cancelled`, also reused on
//! `LifecycleOutcome::Cancelled` for step / span lifecycles when a
//! cancel is observed mid-flight.

use serde::{Deserialize, Serialize};

/// Why this job (or span) was cancelled.
///
/// Independent of `JobStatus::Failed` — `Cancelled` carries product
/// semantics ("interrupted before terminal" / "gave up before finish"),
/// while `Failed` carries error semantics. Cost-attribution and replay
/// UIs treat them differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    /// User typed a new message while a chat job was still running. The
    /// running job's partial output is preserved on
    /// `JobStatus::Cancelled.partial_artifacts`.
    UserPreempt,
    /// Reserved for a future restart-recovery scan: a job that was
    /// `InProgress` at crash time and is rolled to `Cancelled` on the
    /// next process boot. No production code path mints this variant
    /// today — the auto-cancel rewrite at boot was removed (the
    /// `find_recoverable_jobs` listing still exists). The variant is
    /// kept so the wire/serde shape doesn't churn when the rewrite
    /// is restored.
    SystemCrash,
    /// An external `spawn_subagent` subprocess hit its idle safety
    /// timeout (no output within the window). Triggers cancellation of
    /// the entire descendant subtree via the cancellation-token tree.
    SubagentTimeout,
    /// A parent session / job was cancelled and the cancel propagated
    /// down via the cancellation-token tree.
    ParentCancelled,
    /// The parent session was deleted while a subagent was
    /// in-flight. Cancellation propagates first, then the delete is
    /// finalised.
    ParentDeleted,
    /// Human operator initiated the cancel via the admin API or CLI.
    /// Distinct from hook / system-driven cancels so cost-attribution
    /// and replay UIs can split user-initiated work.
    OperatorCancel,
    /// The user ran `/stop` on the session, cancelling the in-flight turn
    /// and every in-flight subagent it spawned. Distinct from
    /// `ParentCancelled` so the subagent wait task can suppress the
    /// terminal `BackgroundJobFinished` delivery (a stopped result must not
    /// repopulate `pending_background_results`).
    UserStopped,
}

impl CancelReason {
    /// Snake-case wire tag matching the serde `rename_all` annotation.
    pub fn as_snake_case(self) -> &'static str {
        match self {
            CancelReason::UserPreempt => "user_preempt",
            CancelReason::SystemCrash => "system_crash",
            CancelReason::SubagentTimeout => "subagent_timeout",
            CancelReason::ParentCancelled => "parent_cancelled",
            CancelReason::ParentDeleted => "parent_deleted",
            CancelReason::OperatorCancel => "operator_cancel",
            CancelReason::UserStopped => "user_stopped",
        }
    }
}
