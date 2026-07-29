//! Lifecycle types for `Step` and `Span` rows.
//!
//! Two concerns, two types:
//!
//! * [`LifecycleOutcome`] — what *happened* once the lifecycle ended.
//!   Has only the three terminal variants. Closing helpers
//!   (`Step::close`, `Span::close`, `with_step`, the agent span guards,
//!   `SpanRecorder::end_step`/`end_span`) take this type, so passing a
//!   non-terminal value is a type error rather than a runtime error.
//!
//! * [`LifecycleState`] — the row's state-machine position. Either
//!   `Pending` (in flight, no outcome yet) or `Done(outcome)`. The
//!   `outcome` field on persisted [`crate::Step`] / [`crate::Span`]
//!   rows is `LifecycleState`.
//!
//! Wire format: `LifecycleState` (de)serializes to the original flat
//! 4-state shape (`{"outcome": "pending|ok|failed|cancelled", ...}`)
//! via [`LifecycleStateRepr`], so on-disk JSON blobs round-trip without
//! a schema migration.

use baybo_turn::CancelReason;
use serde::{Deserialize, Serialize};

/// Terminal outcome of a `Step` or `Span`. Constructed only at
/// close time; the row's pre-close state lives in [`LifecycleState`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleOutcome {
    /// Completed normally. The kind variant's result fields carry the
    /// actual data (LLM tokens, tool output, etc.).
    Ok,
    /// Errored during execution. `reason` is human-readable.
    Failed { reason: String },
    /// Interrupted before completion. `reason` is the same `CancelReason`
    /// taxonomy used at the turn level.
    Cancelled { reason: CancelReason },
}

impl LifecycleOutcome {
    pub fn tag(&self) -> &'static str {
        match self {
            LifecycleOutcome::Ok => "ok",
            LifecycleOutcome::Failed { .. } => "failed",
            LifecycleOutcome::Cancelled { .. } => "cancelled",
        }
    }
}

/// State-machine position of a `Step` or `Span` row. `Pending` until
/// the lifecycle ends, then `Done(outcome)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "LifecycleStateRepr", into = "LifecycleStateRepr")]
pub enum LifecycleState {
    Pending,
    Done(LifecycleOutcome),
}

impl LifecycleState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, LifecycleState::Done(_))
    }

    pub fn tag(&self) -> &'static str {
        match self {
            LifecycleState::Pending => "pending",
            LifecycleState::Done(o) => o.tag(),
        }
    }

    /// Borrow the terminal outcome if this state is `Done`. `None`
    /// while the lifecycle is still in flight.
    pub fn outcome(&self) -> Option<&LifecycleOutcome> {
        match self {
            LifecycleState::Pending => None,
            LifecycleState::Done(o) => Some(o),
        }
    }
}

/// Wire-format proxy for [`LifecycleState`]. Internally tagged by
/// `outcome` so persisted JSON keeps the original 4-state flat shape:
///
/// ```json
/// {"outcome": "pending"}
/// {"outcome": "ok"}
/// {"outcome": "failed", "reason": "..."}
/// {"outcome": "cancelled", "reason": "..."}
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum LifecycleStateRepr {
    Pending,
    Ok,
    Failed { reason: String },
    Cancelled { reason: CancelReason },
}

impl From<LifecycleState> for LifecycleStateRepr {
    fn from(s: LifecycleState) -> Self {
        match s {
            LifecycleState::Pending => Self::Pending,
            LifecycleState::Done(LifecycleOutcome::Ok) => Self::Ok,
            LifecycleState::Done(LifecycleOutcome::Failed { reason }) => Self::Failed { reason },
            LifecycleState::Done(LifecycleOutcome::Cancelled { reason }) => {
                Self::Cancelled { reason }
            }
        }
    }
}

impl From<LifecycleStateRepr> for LifecycleState {
    fn from(r: LifecycleStateRepr) -> Self {
        match r {
            LifecycleStateRepr::Pending => Self::Pending,
            LifecycleStateRepr::Ok => Self::Done(LifecycleOutcome::Ok),
            LifecycleStateRepr::Failed { reason } => {
                Self::Done(LifecycleOutcome::Failed { reason })
            }
            LifecycleStateRepr::Cancelled { reason } => {
                Self::Done(LifecycleOutcome::Cancelled { reason })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_is_in_flight() {
        assert!(!LifecycleState::Pending.is_terminal());
        assert!(LifecycleState::Pending.outcome().is_none());
    }

    #[test]
    fn done_variants_are_terminal() {
        for o in [
            LifecycleOutcome::Ok,
            LifecycleOutcome::Failed { reason: "x".into() },
            LifecycleOutcome::Cancelled {
                reason: CancelReason::SystemCrash,
            },
        ] {
            let state = LifecycleState::Done(o.clone());
            assert!(state.is_terminal());
            assert_eq!(state.outcome(), Some(&o));
        }
    }

    #[test]
    fn round_trips_through_serde_with_legacy_shape() {
        let cases = [
            (LifecycleState::Pending, r#"{"outcome":"pending"}"#),
            (
                LifecycleState::Done(LifecycleOutcome::Ok),
                r#"{"outcome":"ok"}"#,
            ),
            (
                LifecycleState::Done(LifecycleOutcome::Failed {
                    reason: "boom".into(),
                }),
                r#"{"outcome":"failed","reason":"boom"}"#,
            ),
            (
                LifecycleState::Done(LifecycleOutcome::Cancelled {
                    reason: CancelReason::UserPreempt,
                }),
                r#"{"outcome":"cancelled","reason":"user_preempt"}"#,
            ),
        ];
        for (state, expected_json) in cases {
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(json, expected_json, "serialize");
            let back: LifecycleState = serde_json::from_str(&json).unwrap();
            assert_eq!(back, state, "round-trip");
        }
    }

    #[test]
    fn legacy_pending_json_deserializes() {
        // On-disk rows from before the split serialized Pending the
        // same way; this guards the migration-free promise.
        let v: LifecycleState = serde_json::from_str(r#"{"outcome":"pending"}"#).unwrap();
        assert_eq!(v, LifecycleState::Pending);
    }
}
