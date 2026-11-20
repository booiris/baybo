//! Tool-facing ports shared by every kind of human ask: the cancel tokens a
//! parked ask races against, where the ask was raised from, the gauge a
//! tool deadline reads to pause while a human is being waited on, and the
//! question port the `AskUser` tool raises its questions through.
//!
//! The lifecycle behind these ports (persist, enqueue, wake, resolve) lives in
//! `baybo-human-ask`; this crate never depends on it. See
//! `docs/modules/human-ask.md`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use baybo_model::human_ask::{
    AskId, AskPhase, QuestionAnswer, QuestionOutcomeKind, QuestionRequest,
};
use baybo_model::{ChannelType, SessionId, TurnId};
use dashmap::DashMap;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

/// The two cancel tokens a turn runs under. `turn` trips on any cancel of the
/// turn (`/stop`, board stop, operator cancel) and whenever `release` does;
/// `release` trips when the turn is ended with its asks left pending: the
/// actor or the process shutting down, or the idle reaper releasing a turn
/// parked on resumable asks.
#[derive(Debug, Clone)]
pub struct TurnTokens {
    pub turn: CancellationToken,
    pub release: CancellationToken,
}

/// Why a parked ask's turn token tripped. Decides whether the ask is
/// abandoned (`Turn`) or left pending for whoever resumes it — the next
/// process, or a fresh actor through the Resume door (`Ancestor`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOrigin {
    /// The turn itself was cancelled; the actor lives on.
    Turn,
    /// The turn's release token tripped: the turn ends, nothing is appended,
    /// and its asks stay pending.
    Ancestor,
}

impl CancelOrigin {
    /// The one place the origin is decided: `Ancestor` iff the release token
    /// is cancelled.
    pub fn of(tokens: &TurnTokens) -> Self {
        if tokens.release.is_cancelled() {
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
            release: CancellationToken::new(),
        })
    }
}

/// Where an ask was raised: the channel whose queue holds it, the turn that
/// raised it, and the phase of the tool call it blocks. Written onto the
/// ledger row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskSite {
    pub channel: ChannelType,
    pub turn_id: TurnId,
    pub phase: AskPhase,
}

impl AskSite {
    /// A before-execute ask on the owner channel. Tests only.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_test() -> Self {
        Self {
            channel: ChannelType::owner(),
            turn_id: TurnId::default(),
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

/// One set of questions, as the gate persists and shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionAsk {
    pub ask_id: AskId,
    /// `tool_use_id` of the `AskUser` call, so a client badges its step.
    pub tool_call_id: String,
    pub session_id: SessionId,
    pub request: QuestionRequest,
}

/// How a question ask ended for the tool that raised it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionOutcome {
    Answered(Vec<QuestionAnswer>),
    /// The human declined the questions as a whole.
    Declined,
    Cancelled,
    /// Nobody answered: `/stop`, the card was closed, or the ask could not
    /// be shown.
    Abandoned,
}

impl QuestionOutcome {
    /// Coarse outcome kind recorded on the tool span (`SpanEventKind::Question`).
    pub fn kind(&self) -> QuestionOutcomeKind {
        match self {
            Self::Answered(_) => QuestionOutcomeKind::Answered,
            Self::Declined => QuestionOutcomeKind::Declined,
            Self::Cancelled => QuestionOutcomeKind::Cancelled,
            Self::Abandoned => QuestionOutcomeKind::Abandoned,
        }
    }
}

/// Implemented by a channel that can show questions and take their answers.
/// Waits with no timeout; `cancel` is what a parked question races. Unlike
/// [`ApprovalGate`](crate::ApprovalGate) there is no fail-closed fallback: a
/// channel without one simply has no [`QuestionHandle`], and `AskUser`
/// answers that nobody can be asked.
#[async_trait]
pub trait QuestionGate: Send + Sync {
    async fn ask(&self, ask: QuestionAsk, site: AskSite, cancel: AskCancel) -> QuestionOutcome;
}

/// The question gates of the channels that have one, keyed by channel type.
/// Shared by the channel registry (fills it at install) and the tool
/// executor (reads it per call).
#[derive(Default)]
pub struct QuestionGateMap(DashMap<ChannelType, Arc<dyn QuestionGate>>);

impl QuestionGateMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, channel: ChannelType, gate: Arc<dyn QuestionGate>) {
        self.0.insert(channel, gate);
    }

    pub fn remove(&self, channel: &ChannelType) {
        self.0.remove(channel);
    }

    /// `None` when `channel` cannot show questions.
    pub fn get(&self, channel: &ChannelType) -> Option<Arc<dyn QuestionGate>> {
        self.0.get(channel).map(|entry| Arc::clone(entry.value()))
    }
}

/// Every channel's ask gates, as the tool executor reads them per call.
/// Filled by the channel registry at install.
#[derive(Clone, Default)]
pub struct ChannelGates {
    pub approvals: Arc<crate::ApprovalGateMap>,
    pub questions: Arc<QuestionGateMap>,
}

impl ChannelGates {
    pub fn new() -> Self {
        Self::default()
    }
}

/// What a tool asks questions through. The executor wires one into
/// [`crate::ToolContext::question`] only in an attended turn on a channel
/// with a [`QuestionGate`]; `None` means nobody can be asked.
#[derive(Clone)]
pub struct QuestionHandle {
    gate: Arc<dyn QuestionGate>,
    tool_call_id: String,
    session_id: SessionId,
    channel: ChannelType,
    turn_id: TurnId,
    park: ParkGauge,
    cancel: AskCancel,
    last: Arc<Mutex<Option<QuestionOutcomeKind>>>,
}

/// Construction bundle for [`QuestionHandle`]: every field is required.
pub struct QuestionHandleConfig {
    pub gate: Arc<dyn QuestionGate>,
    pub tool_call_id: String,
    pub session_id: SessionId,
    /// The channel whose queue holds the question.
    pub channel: ChannelType,
    pub turn_id: TurnId,
    pub park: ParkGauge,
    pub cancel: AskCancel,
}

impl QuestionHandle {
    pub fn from_config(config: QuestionHandleConfig) -> Self {
        let QuestionHandleConfig {
            gate,
            tool_call_id,
            session_id,
            channel,
            turn_id,
            park,
            cancel,
        } = config;
        Self {
            gate,
            tool_call_id,
            session_id,
            channel,
            turn_id,
            park,
            cancel,
            last: Arc::new(Mutex::new(None)),
        }
    }

    /// Ask `request` and wait for the answer, holding the call's park gauge
    /// so its deadline does not run while a human is being waited on.
    pub async fn ask(&self, request: QuestionRequest) -> QuestionOutcome {
        let ask = QuestionAsk {
            ask_id: AskId::new(),
            tool_call_id: self.tool_call_id.clone(),
            session_id: self.session_id.clone(),
            request,
        };
        let site = AskSite {
            channel: self.channel.clone(),
            turn_id: self.turn_id,
            phase: AskPhase::DuringExecuteResumable,
        };
        let _parked = self.park.park();
        let outcome = self.gate.ask(ask, site, self.cancel.clone()).await;
        *self.last.lock() = Some(outcome.kind());
        outcome
    }

    /// The outcome of the last question raised through this handle, read by
    /// the executor after the tool returns so it can record the span event
    /// and tell a turn-cancelled abandon from a normal answer.
    pub fn last_outcome(&self) -> Option<QuestionOutcomeKind> {
        *self.last.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_is_ancestor_only_when_the_release_token_tripped() {
        let tokens = TurnTokens {
            turn: CancellationToken::new(),
            release: CancellationToken::new(),
        };
        tokens.turn.cancel();
        assert_eq!(CancelOrigin::of(&tokens), CancelOrigin::Turn);
        tokens.release.cancel();
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
