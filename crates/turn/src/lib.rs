//! Turn lifecycle types and orchestration — see `docs/modules/turn.md`
//! for the design.
//!
//! Domain types (`Turn`, `TurnStatus`, `TurnInputKind`, `CancelReason`,
//! `TurnError`) and the `TurnLifecycle` orchestrator both live here; the
//! orchestrator wraps a `TurnStore` with the cancel state machine,
//! lifecycle-event bus, and `TurnId → CancellationToken` registry that the
//! in-flight execution path subscribes to.

mod cancel;
mod cancellation_registry;
mod error;
mod kind;
mod lifecycle;
mod store;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

use baybo_model::{SessionId, SpanId, TriggerKind, TurnId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use baybo_store::{TurnRow, TurnStore};
pub use cancel::CancelReason;
pub use cancellation_registry::{TurnCancellationGuard, TurnCancellationRegistry};
pub use error::TurnError;
pub use kind::{TurnInput, TurnInputKind, TurnOutput};
pub use lifecycle::{TurnLifecycle, TurnLifecycleEvent, TurnPhase};

pub type Result<T> = std::result::Result<T, TurnError>;

// ── TurnStatus ──────────────────────────────────────────────────────

/// Turn lifecycle status.
///
/// ```text
/// Pending → InProgress → Completed
///                    \→ Stuck { reason } → InProgress
///                                       \→ Failed { reason }
///                                       \→ Cancelled { reason, partial_artifacts }
///                    \→ Failed { reason }
///                    \→ Cancelled { reason, partial_artifacts }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TurnStatus {
    Pending,
    InProgress,
    Stuck {
        reason: String,
    },
    Cancelled {
        reason: CancelReason,
        /// Spans that completed (or partially completed) before the
        /// cancel. Reserved for a future prompt-assembly preamble that
        /// surfaces them to the next turn's LLM; no consumer reads this
        /// field today.
        partial_artifacts: Vec<SpanId>,
    },
    Failed {
        reason: String,
    },
    Completed,
}

/// Pure discriminator for `TurnStatus`. Used to express the state
/// machine without having to construct concrete variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TurnStatusKind {
    Pending,
    InProgress,
    Stuck,
    Cancelled,
    Failed,
    Completed,
}

impl TurnStatus {
    pub fn kind(&self) -> TurnStatusKind {
        match self {
            TurnStatus::Pending => TurnStatusKind::Pending,
            TurnStatus::InProgress => TurnStatusKind::InProgress,
            TurnStatus::Stuck { .. } => TurnStatusKind::Stuck,
            TurnStatus::Cancelled { .. } => TurnStatusKind::Cancelled,
            TurnStatus::Failed { .. } => TurnStatusKind::Failed,
            TurnStatus::Completed => TurnStatusKind::Completed,
        }
    }

    pub fn is_terminal(&self) -> bool {
        self.kind().is_terminal()
    }

    pub fn needs_recovery(&self) -> bool {
        self.kind().needs_recovery()
    }
}

impl TurnStatusKind {
    /// Set of statuses reachable from `self` via `Turn::transition`.
    pub fn allowed_transitions(self) -> &'static [TurnStatusKind] {
        use TurnStatusKind::*;
        match self {
            Pending => &[InProgress, Cancelled, Failed],
            InProgress => &[Completed, Stuck, Failed, Cancelled],
            Stuck => &[InProgress, Failed, Cancelled],
            Completed | Failed | Cancelled => &[],
        }
    }

    pub fn can_transition_to(self, target: TurnStatusKind) -> bool {
        self.allowed_transitions().contains(&target)
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TurnStatusKind::Completed | TurnStatusKind::Failed | TurnStatusKind::Cancelled
        )
    }

    pub fn needs_recovery(self) -> bool {
        matches!(
            self,
            TurnStatusKind::Pending | TurnStatusKind::InProgress | TurnStatusKind::Stuck
        )
    }

    /// Snake-case wire tag, matching the serde `rename_all` on
    /// `TurnStatus`. `Display` delegates here so formatted error
    /// messages and JSON wire payloads use the same identifier — no
    /// PascalCase-in-logs / snake_case-in-JSON mismatch.
    pub fn as_snake_case(self) -> &'static str {
        match self {
            TurnStatusKind::Pending => "pending",
            TurnStatusKind::InProgress => "in_progress",
            TurnStatusKind::Stuck => "stuck",
            TurnStatusKind::Cancelled => "cancelled",
            TurnStatusKind::Failed => "failed",
            TurnStatusKind::Completed => "completed",
        }
    }
}

impl std::fmt::Display for TurnStatusKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_snake_case())
    }
}

// ── Turn ─────────────────────────────────────────────────────────────

/// Fallback `origin` for a turn row persisted before the field existed.
fn default_origin() -> TriggerKind {
    TriggerKind::User
}

/// One externally-triggered unit of work. Lives within a `Session` and
/// owns a chain of `Step`s (in `baybo-trace`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub id: TurnId,
    pub session_id: SessionId,
    pub parent_turn_id: Option<TurnId>,

    /// The owning session's root trigger, recorded as-is at creation.
    /// Not asserted against `input` — a `/compact` turn runs inside a
    /// `User`-trigger session and records `origin = User` honestly.
    ///
    /// `serde(default)`: rows persisted before `origin` existed carry no
    /// field (they had a single `kind`). The default lets those rows still
    /// deserialize — `from_row` reads from the `data` blob, so a missing
    /// field would otherwise fail the load (and poison `list_*`, recovery,
    /// and `/stop`).
    #[serde(default = "default_origin")]
    pub origin: TriggerKind,
    pub input: TurnInput,
    pub status: TurnStatus,

    /// Final contractual output. Set when the turn enters `Completed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_result: Option<TurnOutput>,

    /// Index of trace spans during this turn that emitted user-visible
    /// messages. Content lives in the trace tree, not here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emitted_span_ids: Vec<SpanId>,

    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
}

impl Turn {
    /// Construct a fresh turn in `Pending` status. `origin` is the owning
    /// session's root trigger.
    pub fn new(
        session_id: SessionId,
        origin: TriggerKind,
        input: TurnInput,
        parent_turn_id: Option<TurnId>,
    ) -> Self {
        Self {
            id: TurnId::new(),
            session_id,
            parent_turn_id,
            origin,
            input,
            status: TurnStatus::Pending,
            final_result: None,
            emitted_span_ids: Vec::new(),
            created_at: Utc::now(),
            started_at: None,
            ended_at: None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    /// Whether this turn represents a reply being produced — work a user waits
    /// on, and `/stop` can interrupt. `/compact` runs a real turn (to record
    /// compression trace/cost) but produces no reply; a cron-result delivery
    /// appends a reply the fire *already* produced, with no inference of its
    /// own, so there is nothing in flight to wait on or stop.
    pub fn is_chat_turn(&self) -> bool {
        !matches!(
            self.input,
            TurnInput::Compact | TurnInput::CronNotification { .. }
        )
    }

    /// What payload fed this turn — projected from `input`. Display only.
    pub fn input_kind(&self) -> TurnInputKind {
        self.input.input_kind()
    }

    /// Apply a state transition. Validates against the state machine,
    /// mutates status / timestamps / final_result, and returns the
    /// audit record.
    pub fn transition(
        &mut self,
        target: TurnStatus,
        final_result: Option<TurnOutput>,
        reason: Option<String>,
    ) -> Result<TurnTransition> {
        self.transition_at(target, final_result, reason, Utc::now())
    }

    /// Apply a state transition at an explicit point in time. Used by
    /// the boot-time recovery sweep to roll an orphaned `InProgress`
    /// turn to `Cancelled { SystemCrash }` with `ended_at` set to the
    /// last observed activity (`max(child_step.ended_at)`) rather than
    /// the boot wall-clock — the process may have crashed hours or days
    /// before the next start, and using `Utc::now()` here would make
    /// duration metrics meaningless.
    ///
    /// Live callers should keep using [`Self::transition`]; only
    /// recovery code should reach for this variant.
    pub fn transition_at(
        &mut self,
        target: TurnStatus,
        final_result: Option<TurnOutput>,
        reason: Option<String>,
        at: DateTime<Utc>,
    ) -> Result<TurnTransition> {
        let from = self.status.clone();
        if !from.kind().can_transition_to(target.kind()) {
            return Err(TurnError::InvalidTransition(format!(
                "{} -> {} (turn {})",
                from.kind(),
                target.kind(),
                self.id
            )));
        }

        if matches!(target, TurnStatus::InProgress) && self.started_at.is_none() {
            self.started_at = Some(at);
        }
        if target.kind().is_terminal() {
            self.ended_at = Some(at);
        }
        // `final_result` is the contractual output of a successful run.
        // Reject Failed/Cancelled/Stuck targets that try to write one —
        // those carry their reason on the status variant itself; mixing
        // a `final_result` with a non-Completed terminal would corrupt
        // the audit invariant ("`final_result.is_some()` ⇔ `Completed`").
        if let Some(out) = final_result {
            if !matches!(target, TurnStatus::Completed) {
                return Err(TurnError::InvalidTransition(format!(
                    "{} -> {} carries a final_result but only Completed accepts one (turn {})",
                    from.kind(),
                    target.kind(),
                    self.id
                )));
            }
            self.final_result = Some(out);
        }

        let to = target.clone();
        self.status = target;

        Ok(TurnTransition {
            turn_id: self.id,
            from,
            to,
            reason,
            timestamp: at,
        })
    }

    // -- Convenience transition methods --

    pub fn start(&mut self) -> Result<TurnTransition> {
        self.transition(TurnStatus::InProgress, None, None)
    }

    /// Move from `InProgress` to `Completed` with the final contractual
    /// output.
    pub fn complete(&mut self, output: TurnOutput) -> Result<TurnTransition> {
        self.transition(TurnStatus::Completed, Some(output), None)
    }

    pub fn fail(&mut self, reason: impl Into<String>) -> Result<TurnTransition> {
        let reason = reason.into();
        self.transition(
            TurnStatus::Failed {
                reason: reason.clone(),
            },
            None,
            Some(reason),
        )
    }

    pub fn cancel(
        &mut self,
        reason: CancelReason,
        partial_artifacts: Vec<SpanId>,
    ) -> Result<TurnTransition> {
        self.transition(
            TurnStatus::Cancelled {
                reason,
                partial_artifacts,
            },
            None,
            None,
        )
    }

    /// Cancel at an explicit point in time. Used only by the boot-time
    /// recovery sweep — live cancels go through [`Self::cancel`].
    pub fn cancel_at(
        &mut self,
        reason: CancelReason,
        partial_artifacts: Vec<SpanId>,
        at: DateTime<Utc>,
    ) -> Result<TurnTransition> {
        self.transition_at(
            TurnStatus::Cancelled {
                reason,
                partial_artifacts,
            },
            None,
            None,
            at,
        )
    }

    pub fn stuck(&mut self, reason: impl Into<String>) -> Result<TurnTransition> {
        let reason = reason.into();
        self.transition(
            TurnStatus::Stuck {
                reason: reason.clone(),
            },
            None,
            Some(reason),
        )
    }

    /// `Stuck → InProgress` only. Reaching `InProgress` from `Pending` is
    /// the turn of `start()`, which records the transition without a recovery
    /// reason; conflating the two would let `recover()` masquerade as a
    /// regular start and corrupt the recovery audit trail.
    pub fn recover(&mut self, reason: impl Into<String>) -> Result<TurnTransition> {
        if !matches!(self.status, TurnStatus::Stuck { .. }) {
            return Err(TurnError::InvalidTransition(format!(
                "{} -> InProgress (turn {}): recover() requires Stuck",
                self.status.kind(),
                self.id
            )));
        }
        self.transition(TurnStatus::InProgress, None, Some(reason.into()))
    }
}

/// Legality receipt for a single state transition: `Turn`'s edge methods
/// only produce one when the move was valid, so an illegal edge errors
/// before any state changes. The receipt itself is no longer persisted
/// (the `turn_transitions` audit table was retired in the 2026-07
/// unused-column audit — its read API had no reachable surface); the
/// `TurnLifecycle` methods consume and drop it after the edge applies.
#[must_use = "a TurnTransition proves the edge was legal; route state changes through TurnLifecycle rather than discarding the receipt ad hoc"]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnTransition {
    pub turn_id: TurnId,
    pub from: TurnStatus,
    pub to: TurnStatus,
    pub reason: Option<String>,
    pub timestamp: DateTime<Utc>,
}

#[cfg(test)]
#[allow(unused_must_use)] // tests assert on `j.status` directly; the TurnTransition audit record isn't relevant here
mod tests {
    use super::*;
    use baybo_model::ContentBlock;

    fn user_chat_input() -> TurnInput {
        TurnInput::UserChat {
            content: vec![ContentBlock::Text("hi".into())],
        }
    }

    fn fresh_turn() -> Turn {
        Turn::new(
            SessionId::from("cli-test"),
            TriggerKind::User,
            user_chat_input(),
            None,
        )
    }

    fn dummy_output() -> TurnOutput {
        TurnOutput::Message {
            content: vec![ContentBlock::Text("ok".into())],
            ordinal: None,
        }
    }

    // -- TurnStatusKind state machine --

    #[test]
    fn pending_can_start_or_be_cancelled_or_fail() {
        let s = TurnStatusKind::Pending;
        assert!(s.can_transition_to(TurnStatusKind::InProgress));
        assert!(s.can_transition_to(TurnStatusKind::Cancelled));
        assert!(s.can_transition_to(TurnStatusKind::Failed));
        assert!(!s.can_transition_to(TurnStatusKind::Completed));
        assert!(!s.can_transition_to(TurnStatusKind::Stuck));
    }

    #[test]
    fn in_progress_transitions() {
        let s = TurnStatusKind::InProgress;
        assert!(s.can_transition_to(TurnStatusKind::Completed));
        assert!(s.can_transition_to(TurnStatusKind::Stuck));
        assert!(s.can_transition_to(TurnStatusKind::Failed));
        assert!(s.can_transition_to(TurnStatusKind::Cancelled));
        assert!(!s.can_transition_to(TurnStatusKind::Pending));
    }

    #[test]
    fn terminal_kinds_have_no_transitions() {
        assert!(TurnStatusKind::Completed.allowed_transitions().is_empty());
        assert!(TurnStatusKind::Failed.allowed_transitions().is_empty());
        assert!(TurnStatusKind::Cancelled.allowed_transitions().is_empty());
    }

    #[test]
    fn is_terminal_and_needs_recovery_are_complementary() {
        for k in [
            TurnStatusKind::Pending,
            TurnStatusKind::InProgress,
            TurnStatusKind::Stuck,
            TurnStatusKind::Cancelled,
            TurnStatusKind::Failed,
            TurnStatusKind::Completed,
        ] {
            assert_ne!(k.is_terminal(), k.needs_recovery());
        }
    }

    // -- Turn::new --

    #[test]
    fn new_turn_is_pending() {
        let j = fresh_turn();
        assert!(matches!(j.status, TurnStatus::Pending));
        assert_eq!(j.input_kind(), TurnInputKind::UserChat);
        assert_eq!(j.origin, TriggerKind::User);
        assert!(j.is_chat_turn());
        assert!(j.started_at.is_none());
        assert!(j.ended_at.is_none());
        assert!(j.final_result.is_none());
        assert!(j.parent_turn_id.is_none());
    }

    #[test]
    fn new_turns_have_unique_ids() {
        let a = fresh_turn();
        let b = fresh_turn();
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn origin_and_input_are_independent() {
        // A spawned-input turn inside a cron-trigger session keeps all
        // facts separately: input kind = Spawned, origin = Cron.
        let j = Turn::new(
            SessionId::from("s"),
            TriggerKind::Cron,
            TurnInput::Spawned {
                initial_prompt: vec![],
            },
            None,
        );
        assert_eq!(j.input_kind(), TurnInputKind::Spawned);
        assert_eq!(j.origin, TriggerKind::Cron);
        assert!(j.is_chat_turn());
    }

    #[test]
    fn compact_input_is_not_a_turn() {
        let j = Turn::new(
            SessionId::from("s"),
            TriggerKind::User,
            TurnInput::Compact,
            None,
        );
        assert!(!j.is_chat_turn());
        assert_eq!(j.input_kind(), TurnInputKind::Compact);
        assert_eq!(j.origin, TriggerKind::User);
    }

    // -- Happy path --

    #[test]
    fn full_success_path() {
        let mut j = fresh_turn();
        let t = j.start().unwrap();
        assert_eq!(t.from, TurnStatus::Pending);
        assert!(matches!(t.to, TurnStatus::InProgress));
        assert!(j.started_at.is_some());

        let t = j.complete(dummy_output()).unwrap();
        assert!(matches!(t.from, TurnStatus::InProgress));
        assert!(matches!(j.status, TurnStatus::Completed));
        assert!(j.is_terminal());
        assert!(j.ended_at.is_some());
        assert!(j.final_result.is_some());
    }

    #[test]
    fn fail_from_in_progress() {
        let mut j = fresh_turn();
        j.start().unwrap();
        let t = j.fail("timeout").unwrap();
        assert!(matches!(t.to, TurnStatus::Failed { .. }));
        assert!(j.is_terminal());
        assert!(j.ended_at.is_some());
    }

    #[test]
    fn cancel_from_in_progress_keeps_partial() {
        let mut j = fresh_turn();
        j.start().unwrap();
        let span = SpanId::new();
        j.cancel(CancelReason::UserPreempt, vec![span]).unwrap();
        match &j.status {
            TurnStatus::Cancelled {
                reason,
                partial_artifacts,
            } => {
                assert_eq!(*reason, CancelReason::UserPreempt);
                assert_eq!(partial_artifacts.as_slice(), &[span]);
            }
            _ => panic!("expected Cancelled"),
        }
        assert!(j.is_terminal());
    }

    #[test]
    fn cancel_from_pending() {
        let mut j = fresh_turn();
        j.cancel(CancelReason::ParentDeleted, vec![]).unwrap();
        assert!(matches!(j.status, TurnStatus::Cancelled { .. }));
    }

    #[test]
    fn stuck_then_recover() {
        let mut j = fresh_turn();
        j.start().unwrap();
        j.stuck("hung").unwrap();
        assert!(matches!(j.status, TurnStatus::Stuck { .. }));
        let t = j.recover("watchdog").unwrap();
        assert!(matches!(t.to, TurnStatus::InProgress));
        assert_eq!(t.reason.as_deref(), Some("watchdog"));
    }

    #[test]
    fn stuck_then_cancel() {
        let mut j = fresh_turn();
        j.start().unwrap();
        j.stuck("hung").unwrap();
        j.cancel(CancelReason::ParentCancelled, vec![]).unwrap();
        assert!(matches!(j.status, TurnStatus::Cancelled { .. }));
    }

    #[test]
    fn cannot_complete_from_pending() {
        let mut j = fresh_turn();
        let err = j.complete(dummy_output()).unwrap_err();
        assert!(matches!(err, TurnError::InvalidTransition(_)));
    }

    #[test]
    fn recover_rejects_pending() {
        let mut j = fresh_turn();
        let err = j.recover("oops").unwrap_err();
        assert!(matches!(err, TurnError::InvalidTransition(_)));
        assert!(matches!(j.status, TurnStatus::Pending));
    }

    #[test]
    fn recover_rejects_in_progress() {
        let mut j = fresh_turn();
        j.start().unwrap();
        let err = j.recover("oops").unwrap_err();
        assert!(matches!(err, TurnError::InvalidTransition(_)));
        assert!(matches!(j.status, TurnStatus::InProgress));
    }

    #[test]
    fn cannot_transition_from_terminal() {
        let mut j = fresh_turn();
        j.start().unwrap();
        j.fail("done").unwrap();
        let err = j.start().unwrap_err();
        assert!(matches!(err, TurnError::InvalidTransition(_)));
    }

    // -- Serde --

    #[test]
    fn turn_round_trips_through_serde() {
        let mut j = fresh_turn();
        j.start().unwrap();
        j.complete(dummy_output()).unwrap();
        let s = serde_json::to_string(&j).unwrap();
        let back: Turn = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, j.id);
        assert_eq!(back.origin, j.origin);
        assert_eq!(back.input_kind(), j.input_kind());
        assert_eq!(back.session_id, j.session_id);
    }

    #[test]
    fn row_without_origin_deserializes_with_default() {
        // A row persisted before `origin` existed carries no field. It must
        // still load — `from_row` reads the whole `Turn` from the `data` blob,
        // so a missing required field would otherwise fail the load and
        // poison `list_*` / recovery / `/stop`.
        let j = fresh_turn();
        let mut v = serde_json::to_value(&j).unwrap();
        let obj = v.as_object_mut().expect("turn serializes to an object");
        obj.remove("origin");
        // The dropped legacy field is now an unknown key — serde ignores it.
        obj.insert("kind".into(), serde_json::json!("user_chat"));

        let back: Turn = serde_json::from_value(v).expect("legacy row must deserialize");
        assert_eq!(back.id, j.id);
        assert_eq!(back.origin, TriggerKind::User, "default_origin");
        assert!(back.is_chat_turn());
    }

    #[test]
    fn turn_status_round_trips_through_serde() {
        let s = TurnStatus::Cancelled {
            reason: CancelReason::SystemCrash,
            partial_artifacts: vec![SpanId::new()],
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: TurnStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind(), s.kind());
    }
}
