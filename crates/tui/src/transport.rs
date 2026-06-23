//! TUI-facing event shape for the WS transport pump.
//!
//! [`TransportEvent`] is the normalized stream item the TUI's event
//! loop consumes — the concrete [`crate::client::WsTransport`] maps
//! `baybo-channels` wire frames onto these variants so the run loop
//! doesn't need to know about the underlying MessagePack protocol.

use std::pin::Pin;

use baybo_channels::{NoticeLevel, Result};
use baybo_model::ContentBlock;
use baybo_tools::ApprovalDecision;
use futures::Stream;

/// Event shape surfaced by [`crate::client::WsTransport::subscribe`].
/// Kept as a public enum so tests (and any future alternate transport)
/// can build one without going through the WS stack.
#[derive(Debug, Clone)]
pub enum TransportEvent {
    /// Incremental assistant text chunk.
    StreamDelta(String),
    /// A tool call started; `label` is the optional human preview. `call_id`
    /// keys it to its eventual completion.
    ToolStarted {
        call_id: String,
        tool: String,
        label: Option<String>,
    },
    /// A tool call finished. `status` is the wire string (`"ok"` /
    /// `"error"` / `"denied"`); `summary` is the short result rendering.
    ToolCompleted {
        call_id: String,
        status: String,
        summary: String,
    },
    /// A coarse turn-phase transition (today `"compacting"` /
    /// `"compacted"` for context compaction start/end).
    Status { phase: String },
    /// Final assistant response for the turn.
    Response(Vec<ContentBlock>),
    /// Out-of-band notice surfaced by the agent.
    Notice { level: NoticeLevel, text: String },
    /// A new tool-call approval was queued on the shared
    /// [`baybo_tools::ApprovalQueue`]; the loop should refresh the
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
