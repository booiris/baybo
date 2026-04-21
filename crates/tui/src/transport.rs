//! TUI-facing event shape for the WS transport pump.
//!
//! [`TransportEvent`] is the normalised stream item the TUI's event
//! loop consumes — the concrete [`crate::client::WsTransport`] maps
//! `aura-channels` wire frames onto these variants so the run loop
//! doesn't need to know about the underlying MessagePack protocol.

use std::pin::Pin;

use aura_channels::{NoticeLevel, Result};
use aura_model::ContentBlock;
use aura_tools::ApprovalDecision;
use futures::Stream;

/// Event shape surfaced by [`crate::client::WsTransport::subscribe`].
/// Kept as a public enum so tests (and any future alternate transport)
/// can build one without going through the WS stack.
#[derive(Debug, Clone)]
pub enum TransportEvent {
    /// Incremental assistant text chunk.
    StreamDelta(String),
    /// Final assistant response for the turn.
    Response(Vec<ContentBlock>),
    /// Out-of-band notice surfaced by the agent.
    Notice { level: NoticeLevel, text: String },
    /// A new tool-call approval was queued on the shared
    /// [`aura_tools::ApprovalQueue`]; the loop should refresh the
    /// approval UI.
    ApprovalRequested,
    /// An approval was resolved remotely (by another client); the loop
    /// should drop any pending UI entry for `call_id`.
    ApprovalResolved {
        call_id: String,
        decision: ApprovalDecision,
    },
}

/// Stream of [`TransportEvent`]s produced by [`crate::client::WsTransport::subscribe`].
pub type TransportEventStream = Pin<Box<dyn Stream<Item = Result<TransportEvent>> + Send>>;
