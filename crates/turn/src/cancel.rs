//! Cancellation reasons for `TurnStatus::Cancelled`, also reused on
//! `LifecycleOutcome::Cancelled` for step / span lifecycles when a
//! cancel is observed mid-flight.

use serde::{Deserialize, Serialize};

/// Why this turn (or span) was cancelled.
///
/// Independent of `TurnStatus::Failed` — `Cancelled` carries product
/// semantics ("interrupted before terminal" / "gave up before finish"),
/// while `Failed` carries error semantics. Cost-attribution and replay
/// UIs treat them differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    /// User typed a new message while a chat turn was still running. The
    /// running turn's partial output is preserved on
    /// `TurnStatus::Cancelled.partial_artifacts`.
    UserPreempt,
    /// The process or actor that owned this turn crashed before it could
    /// close the normal lifecycle path. Boot recovery and in-process actor
    /// panic recovery roll such turns to `Cancelled { SystemCrash }`.
    SystemCrash,
    /// An external `spawn_subagent` subprocess hit its idle safety
    /// timeout (no output within the window). Triggers cancellation of
    /// the entire descendant subtree via the cancellation-token tree.
    SubagentTimeout,
    /// A parent session / turn was cancelled and the cancel propagated
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
    /// The board this run belongs to crossed its money ceiling while the
    /// turn was still working. Nobody asked for it, so it is not an
    /// `OperatorCancel`; the work itself was fine, so it is not a failure.
    BudgetExhausted,
    /// The user ran `/stop` on the session, cancelling the in-flight turn
    /// and every in-flight subagent it spawned. Distinct from
    /// `ParentCancelled` so the subagent wait task can suppress the
    /// terminal `BackgroundJobFinished` delivery (a stopped result must not
    /// repopulate the parent session's background-notification buffer).
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
            CancelReason::BudgetExhausted => "budget_exhausted",
            CancelReason::UserStopped => "user_stopped",
        }
    }
}
