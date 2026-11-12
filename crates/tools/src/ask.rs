//! Tool-facing ports shared by every kind of human ask: the cancel tokens a
//! parked ask races against, where the ask was raised from, and the gauge a
//! tool deadline reads to pause while a human is being waited on.
//!
//! The lifecycle behind these ports (persist, enqueue, wake, resolve) lives in
//! `baybo-human-ask`; this crate never depends on it. See
//! `docs/modules/human-ask.md`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use baybo_model::ChannelType;
use baybo_model::human_ask::AskPhase;
use tokio_util::sync::CancellationToken;

/// The two cancel tokens a turn runs under. `turn` trips on any cancel of the
/// turn (`/stop`, board stop, operator cancel) and on every ancestor cancel;
/// `actor` trips only when the actor itself is being torn down (process
/// shutdown).
#[derive(Debug, Clone)]
pub struct TurnTokens {
    pub turn: CancellationToken,
    pub actor: CancellationToken,
}

/// Why a parked ask's turn token tripped. Decides whether the ask is
/// abandoned (`Turn`) or left pending for the next process (`Ancestor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOrigin {
    /// The turn itself was cancelled; the actor lives on.
    Turn,
    /// The actor's token tripped — the process is going away.
    Ancestor,
}

impl CancelOrigin {
    /// The one place the origin is decided: `Ancestor` iff the actor token is
    /// cancelled.
    pub fn of(tokens: &TurnTokens) -> Self {
        if tokens.actor.is_cancelled() {
            Self::Ancestor
        } else {
            Self::Turn
        }
    }
}

/// What an ask races its wait against.
#[derive(Debug, Clone)]
pub struct AskCancel {
    pub tokens: TurnTokens,
}

impl AskCancel {
    pub fn new(tokens: TurnTokens) -> Self {
        Self { tokens }
    }

    /// The origin of a cancel that has already tripped `tokens.turn`.
    pub fn origin(&self) -> CancelOrigin {
        CancelOrigin::of(&self.tokens)
    }

    /// Tokens nobody will ever trip. Tests only.
    #[cfg(any(test, feature = "test-support"))]
    pub fn never() -> Self {
        Self::new(TurnTokens {
            turn: CancellationToken::new(),
            actor: CancellationToken::new(),
        })
    }
}

/// Where an ask was raised: the channel whose queue holds it and the phase of
/// the tool call it blocks. Written onto the ledger row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskSite {
    pub channel: ChannelType,
    pub phase: AskPhase,
}

impl AskSite {
    /// A before-execute ask on the owner channel. Tests only.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test() -> Self {
        Self {
            channel: ChannelType::owner(),
            phase: AskPhase::BeforeExecute,
        }
    }
}

/// Counts the asks a tool call is currently parked on. Shared by the call's
/// handles so the executor's deadline can stop accruing while a human is
/// being waited on.
#[derive(Debug, Clone, Default)]
pub struct ParkGauge(Arc<AtomicUsize>);

impl ParkGauge {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark one ask parked until the returned guard drops.
    pub fn park(&self) -> ParkGuard {
        self.0.fetch_add(1, Ordering::AcqRel);
        ParkGuard(Arc::clone(&self.0))
    }

    pub fn is_parked(&self) -> bool {
        self.0.load(Ordering::Acquire) > 0
    }
}

/// RAII half of [`ParkGauge::park`]; decrements on drop, including when the
/// parked future is dropped mid-wait.
#[derive(Debug)]
pub struct ParkGuard(Arc<AtomicUsize>);

impl Drop for ParkGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_is_ancestor_only_when_the_actor_token_tripped() {
        let tokens = TurnTokens {
            turn: CancellationToken::new(),
            actor: CancellationToken::new(),
        };
        tokens.turn.cancel();
        assert_eq!(CancelOrigin::of(&tokens), CancelOrigin::Turn);
        tokens.actor.cancel();
        assert_eq!(CancelOrigin::of(&tokens), CancelOrigin::Ancestor);
    }

    #[test]
    fn park_guard_counts_and_releases() {
        let gauge = ParkGauge::new();
        assert!(!gauge.is_parked());
        let a = gauge.park();
        let b = gauge.clone().park();
        assert!(gauge.is_parked());
        drop(a);
        assert!(gauge.is_parked());
        drop(b);
        assert!(!gauge.is_parked());
    }
}
