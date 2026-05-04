//! Lifecycle outcome shared by `Step` and `Span`.

use aura_job::CancelReason;
use serde::{Deserialize, Serialize};

/// The state of a `Step` or `Span` w.r.t. completion.
///
/// Spans / steps start in `Pending` (i.e. `started_at` set, no outcome
/// recorded yet). They transition to exactly one of the three terminal
/// outcomes when they end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum LifecycleOutcome {
    /// Started but not yet ended. Persisted on the main `steps` /
    /// `spans` row with `ended_at = NULL`; the row is rewritten in
    /// place when the lifecycle ends with one of the terminal variants
    /// below.
    Pending,
    /// Completed normally. The kind variant's result fields carry the
    /// actual data (LLM tokens, tool output, etc.).
    Ok,
    /// Errored during execution. `reason` is human-readable.
    Failed { reason: String },
    /// Interrupted before completion. `reason` is the same `CancelReason`
    /// taxonomy used at the job level.
    Cancelled { reason: CancelReason },
}

impl LifecycleOutcome {
    /// Whether this outcome represents the end of a step / span.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, LifecycleOutcome::Pending)
    }

    pub fn tag(&self) -> &'static str {
        match self {
            LifecycleOutcome::Pending => "pending",
            LifecycleOutcome::Ok => "ok",
            LifecycleOutcome::Failed { .. } => "failed",
            LifecycleOutcome::Cancelled { .. } => "cancelled",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_is_in_flight() {
        assert!(!LifecycleOutcome::Pending.is_terminal());
    }

    #[test]
    fn ok_failed_cancelled_are_terminal() {
        assert!(LifecycleOutcome::Ok.is_terminal());
        assert!(LifecycleOutcome::Failed { reason: "x".into() }.is_terminal());
        assert!(
            LifecycleOutcome::Cancelled {
                reason: CancelReason::SystemCrash,
            }
            .is_terminal()
        );
    }

    #[test]
    fn round_trips_through_serde() {
        for o in [
            LifecycleOutcome::Pending,
            LifecycleOutcome::Ok,
            LifecycleOutcome::Failed {
                reason: "boom".into(),
            },
            LifecycleOutcome::Cancelled {
                reason: CancelReason::UserPreempt,
            },
        ] {
            let s = serde_json::to_string(&o).unwrap();
            let back: LifecycleOutcome = serde_json::from_str(&s).unwrap();
            assert_eq!(back, o);
        }
    }
}
