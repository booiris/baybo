//! Pluggable long-term memory subsystem.
//!
//! The system knows memory only through a single [`Memory`] trait object
//! (`Arc<dyn Memory>`), not a many-registry: at most one implementation is
//! registered at startup. The trait is intentionally thin and
//! **storage-opaque** — an implementation owns its own persistence (libsql, a
//! vector DB, an external service) and receives its LLM + embedding handles and
//! config in its own constructor.
//!
//! Core ships the trait, its value types, and a [`NoopMemory`] default that
//! does nothing; there is no real implementation here. The agent loop drives it
//! through three hooks — a synchronous [`Memory::recall`] at job start and on
//! each interjection, and the fire-and-forget [`Memory::on_job_complete`] /
//! [`Memory::on_session_end`] events — plus the tools the impl contributes via
//! [`Memory::tools`].
//!
//! Recall results are injected into the prompt as a framed, persisted
//! [`aura_model::MessageSource::RecalledMemory`] row, never a `Role::System`
//! message — see `docs/modules/memory.md` for the hard constraints carried
//! forward from the retired heuristic pipeline.

mod error;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

use std::sync::Arc;

use async_trait::async_trait;
use aura_llm::Attribution;
use aura_model::{ChatMessage, ContentBlock};
use aura_tools::{Tool, ToolManifest};

pub use error::MemoryError;

pub type Result<T> = std::result::Result<T, MemoryError>;

/// Per-call context the core mints for every [`Memory`] call. Carries the
/// [`Attribution`] the impl binds its LLM + embedding handles with, so memory
/// spend bills to the real user/session and joins the `MemoryRecall` /
/// `MemoryWrite` trace step (mirrors compression's attribution binding). The
/// attribution also carries the user/session/job/span ids an impl needs to
/// scope its own per-session or per-job de-duplication.
pub struct MemoryContext {
    pub attribution: Attribution,
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
    /// job start and on each interjection, so it sits on the critical path —
    /// the impl must keep it fast. De-duplication against memories already
    /// surfaced in this session/job is INTERNAL to the impl (keyed off `ctx`);
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
    async fn on_job_complete(
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

    async fn on_job_complete(
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
