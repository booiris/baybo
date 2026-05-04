//! Cancellation reasons for `JobStatus::Cancelled` and (reused) for
//! span-level cancellation when the recovery scan finds half-open spans.

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
    /// Process restart found a job that was `InProgress` at crash time.
    /// The recovery scan rewrites half-open spans with the same reason
    /// and folds them into the parent job's `partial_artifacts`.
    SystemCrash,
    /// `spawn_subagent` exceeded its declared timeout. Triggers
    /// cancellation of the entire descendant subtree via the
    /// cancellation-token tree.
    SubagentTimeout,
    /// A parent session / job was cancelled and the cancel propagated
    /// down via the cancellation-token tree.
    ParentCancelled,
    /// The parent session was soft-deleted while a subagent was
    /// in-flight. Cancellation propagates first, then the soft-delete
    /// is finalised.
    ParentDeleted,
    /// A `PreStep` hook returned `Abort` for this step's surrounding
    /// job. See the hook-router protocol in `agent.md`.
    HookAborted,
    /// Human operator initiated the cancel via the admin API or CLI.
    /// Distinct from hook / system-driven cancels so cost-attribution
    /// and replay UIs can split user-initiated work.
    OperatorCancel,
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
            CancelReason::HookAborted => "hook_aborted",
            CancelReason::OperatorCancel => "operator_cancel",
        }
    }
}
