//! The hydration-time question "which of this session's `tool_use` blocks are
//! still waiting on a human?" — answered by whoever owns the ask ledger.
//!
//! Hydration must not fill such a call with the synthetic "interrupted"
//! result: its real result arrives when the answer is applied, and it has to
//! land adjacent to the issuing assistant row.

use async_trait::async_trait;
use baybo_model::SessionId;

#[async_trait]
pub trait PendingToolCalls: Send + Sync {
    /// The `tool_use` ids of this session whose ask is not applied yet and
    /// will be settled by a later turn. Errors answer an empty list: the
    /// fallback is today's synthetic fill.
    async fn unapplied_tool_use_ids(&self, session: &SessionId) -> Vec<String>;
}

/// A session with no ask ledger behind it: every dangling call is filled.
#[cfg(any(test, feature = "test-support"))]
pub struct NoPendingToolCalls;

#[cfg(any(test, feature = "test-support"))]
#[async_trait]
impl PendingToolCalls for NoPendingToolCalls {
    async fn unapplied_tool_use_ids(&self, _session: &SessionId) -> Vec<String> {
        Vec::new()
    }
}
