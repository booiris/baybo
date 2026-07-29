//! Pluggable long-term memory subsystem.
//!
//! The system knows memory only through a single [`Memory`] trait object
//! (`Arc<dyn Memory>`), not a many-registry: at most one implementation is
//! registered at startup. The trait is intentionally thin and
//! **storage-opaque** — an implementation owns its own persistence (sqlite, a
//! vector DB, an external service) and receives its LLM handle and config in
//! its own constructor.
//!
//! Core ships the trait, its value types, and a [`NoopMemory`] default that
//! does nothing; there is no real implementation here. The agent loop drives it
//! through three hooks — a synchronous [`Memory::recall`] at turn start and on
//! each interjection, and the fire-and-forget [`Memory::on_turn_complete`] /
//! [`Memory::on_session_end`] events — plus the tools the impl contributes via
//! [`Memory::tools`].
//!
//! Recall results are injected into the prompt as a framed, persisted
//! [`baybo_model::MessageSource::RecalledMemory`] row, never a `Role::System`
//! message — see `docs/modules/memory.md` for the hard constraints carried
//! forward from the retired heuristic pipeline.

mod error;

pub mod backends;
pub mod boot;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

use std::sync::Arc;

use async_trait::async_trait;
use baybo_llm::Attribution;
use baybo_model::{ChatMessage, ContentBlock, SessionId, TurnId};
use baybo_tools::{Tool, ToolManifest};
use baybo_trace::{
    LifecycleOutcome, LlmCallBegin, LlmCallResult, SpanFinalize, SpanKind, SpanRecorder, StepHandle,
};

pub use error::MemoryError;

pub type Result<T> = std::result::Result<T, MemoryError>;

/// Per-call context the core builds for every [`Memory`] call. Carries the
/// real `(user, session, turn)` this operation belongs to, plus the trace
/// recorder and the enclosing `MemoryRecall` / `MemoryWrite` step.
///
/// An impl that makes billed LLM sub-calls runs each through
/// [`MemoryContext::scoped_llm_call`], which opens an `LlmCall` span **under the
/// memory step** and hands back an [`Attribution`] bound to that span — so the
/// spend the call records lands on a real span attributed to the real
/// user/session/turn, never an orphaned id. The same `(user, session, turn)` ids
/// (via [`Self::session_id`] / [`Self::turn_id`]) are what an impl keys its own
/// per-session / per-turn de-duplication off.
pub struct MemoryContext {
    user_id: String,
    session_id: SessionId,
    turn_id: TurnId,
    recorder: Arc<SpanRecorder>,
    step: StepHandle,
}

impl MemoryContext {
    /// Build the context for one memory call. The core calls this **inside** the
    /// `MemoryRecall` / `MemoryWrite` trace step, so `step` is that step and
    /// every [`Self::scoped_llm_call`] span nests under it.
    pub fn new(
        user_id: String,
        session_id: SessionId,
        turn_id: TurnId,
        recorder: Arc<SpanRecorder>,
        step: StepHandle,
    ) -> Self {
        Self {
            user_id,
            session_id,
            turn_id,
            recorder,
            step,
        }
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    /// Record a billed LLM sub-call as an `LlmCall` span under this operation's
    /// memory step, so the spend it records attributes to a **real** span (and
    /// the real user/session/turn), not an orphaned id. The closure receives the
    /// [`Attribution`] bound to that span — bind your billed client with it,
    /// make the call, and return its [`LlmCallResult`] (token usage, for the
    /// span) alongside your value. Mirrors the agent loop's own LLM-span
    /// discipline.
    pub async fn scoped_llm_call<F, Fut, T>(&self, begin: LlmCallBegin, body: F) -> Result<T>
    where
        F: FnOnce(Attribution) -> Fut,
        Fut: std::future::Future<Output = (LlmCallResult, Result<T>)>,
    {
        let span = self
            .recorder
            .begin_span(
                &self.step,
                SpanKind::LlmCall {
                    begin,
                    result: None,
                },
                None,
            )
            .await
            .map_err(|e| MemoryError::Internal(anyhow::Error::new(e)))?;
        let attribution = Attribution {
            user_id: self.user_id.clone(),
            session_id: self.session_id.clone(),
            turn_id: self.turn_id,
            span_id: span.span_id,
            reason: baybo_llm::CallReason::Memory,
        };
        let (call_result, value) = body(attribution).await;
        let outcome = match &value {
            Ok(_) => LifecycleOutcome::Ok,
            Err(e) => LifecycleOutcome::Failed {
                reason: e.to_string(),
            },
        };
        if let Err(e) = self
            .recorder
            .end_span(
                span,
                self.turn_id,
                SpanFinalize::LlmCall(call_result),
                outcome,
            )
            .await
        {
            tracing::warn!(error = %e, "memory: end_span failed; span left half-open");
        }
        value
    }
}

/// One memory the core injects into the `<recalled_memory>` envelope. A struct
/// (not a bare `String`) for forward-compat — e.g. an optional category the
/// envelope could later render. No id: de-duplication against already-surfaced
/// memories is internal to the implementation.
#[derive(Debug, Clone)]
pub struct RecalledMemory {
    pub content: String,
}

/// The single pluggable memory contract. One implementation is registered as
/// `Arc<dyn Memory>`; everything beyond this surface is opaque to the core.
#[async_trait]
pub trait Memory: Send + Sync {
    /// Synchronous query: the memories relevant to `query`. Called inline at
    /// turn start and on each interjection, so it sits on the critical path —
    /// the impl must keep it fast. De-duplication against memories already
    /// surfaced in this session/turn is INTERNAL to the impl (keyed off `ctx`);
    /// because one impl is a process singleton, that state survives actor
    /// reap/rehydration for free. The core injects exactly what is returned and
    /// performs no de-duplication of its own.
    async fn recall(
        &self,
        ctx: &MemoryContext,
        query: &[ContentBlock],
    ) -> Result<Vec<RecalledMemory>>;

    /// Background event: one finished exchange. `user_input` includes any
    /// mid-turn interjections; `final_output` is the assistant's last turn. The
    /// impl decides what (if anything) to extract and store — the core makes no
    /// assumptions and never treats the whole output as a memory.
    async fn on_turn_complete(
        &self,
        ctx: &MemoryContext,
        user_input: &[ContentBlock],
        final_output: &[ContentBlock],
    ) -> Result<()>;

    /// Background event: whole-session consolidation at idle-timeout, with the
    /// FULL durable transcript (the in-memory view may have been compressed).
    async fn on_session_end(&self, ctx: &MemoryContext, transcript: &[ChatMessage]) -> Result<()>;

    /// Tools this implementation contributes to the agent registry — the
    /// model's "explicit signal" path, coexisting with the automatic
    /// recall/write path. Built pre-wired to the impl's own state and clients,
    /// and registered statically at startup. Defaults to none.
    fn tools(&self) -> Vec<(Arc<dyn Tool>, ToolManifest)> {
        Vec::new()
    }
}

/// Default no-op memory: recalls nothing, stores nothing, contributes no tools.
/// The reference implementation of the contract and the default the runtime
/// wires until a real backend is plugged in (also handy as a test fixture), so
/// the agent-loop hooks have something to call.
pub struct NoopMemory;

#[async_trait]
impl Memory for NoopMemory {
    async fn recall(
        &self,
        _ctx: &MemoryContext,
        _query: &[ContentBlock],
    ) -> Result<Vec<RecalledMemory>> {
        Ok(Vec::new())
    }

    async fn on_turn_complete(
        &self,
        _ctx: &MemoryContext,
        _user_input: &[ContentBlock],
        _final_output: &[ContentBlock],
    ) -> Result<()> {
        Ok(())
    }

    async fn on_session_end(
        &self,
        _ctx: &MemoryContext,
        _transcript: &[ChatMessage],
    ) -> Result<()> {
        Ok(())
    }
}

/// Recall-only wrapper: forwards `recall` + `tools` to the inner backend but
/// no-ops the write hooks (`on_turn_complete` / `on_session_end`). Wired into
/// `baybo`'s runtime only under the `bench-readonly-memory` feature, so the
/// memory benchmark can drive the real agent end-to-end (recall + tools live)
/// without QA turns writing into — and polluting — the recall scope. NOT a
/// production path.
#[cfg(feature = "bench-readonly-memory")]
pub struct ReadOnlyMemory(Arc<dyn Memory>);

#[cfg(feature = "bench-readonly-memory")]
impl ReadOnlyMemory {
    pub fn new(inner: Arc<dyn Memory>) -> Self {
        Self(inner)
    }
}

#[cfg(feature = "bench-readonly-memory")]
#[async_trait]
impl Memory for ReadOnlyMemory {
    async fn recall(
        &self,
        ctx: &MemoryContext,
        query: &[ContentBlock],
    ) -> Result<Vec<RecalledMemory>> {
        self.0.recall(ctx, query).await
    }

    async fn on_turn_complete(
        &self,
        _ctx: &MemoryContext,
        _user_input: &[ContentBlock],
        _final_output: &[ContentBlock],
    ) -> Result<()> {
        Ok(())
    }

    async fn on_session_end(
        &self,
        _ctx: &MemoryContext,
        _transcript: &[ChatMessage],
    ) -> Result<()> {
        Ok(())
    }

    fn tools(&self) -> Vec<(Arc<dyn Tool>, ToolManifest)> {
        // Read-only mode must not hand the agent mutating tools (mem0_add,
        // viking_store, …): a QA turn could invoke one and write into the recall
        // scope, contaminating later questions — exactly what this wrapper exists
        // to prevent. Recall is auto-injected, so the benchmark agent needs no
        // memory tools at all.
        Vec::new()
    }
}
