//! Abstract transport seam for the TUI.
//!
//! The TUI hands the transport an [`IncomingMessage`] and receives a
//! stream of already-translated [`TransportEvent`]s to feed the event
//! loop. Implementations live outside this module — the production
//! impl is [`super::client::GatewayTransport`], and tests may supply
//! their own.

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use aura_channels::{IncomingMessage, NoticeLevel, Result};
use aura_model::ContentBlock;
use aura_tools::{ApprovalDecision, ApprovalQueue};
use futures::Stream;

/// Public event shape produced by a [`TuiTransport`]. The adapter
/// translates these into its internal `AppEvent` variants; keeping the
/// wire-facing shape in a public enum lets transport implementations
/// live outside the crate boundary (or be mocked in tests) without
/// exposing internal state.
#[derive(Debug, Clone)]
pub enum TransportEvent {
    /// Incremental assistant text chunk.
    StreamDelta(String),
    /// Final assistant response for the turn.
    Response(Vec<ContentBlock>),
    /// Out-of-band notice surfaced by the agent.
    Notice { level: NoticeLevel, text: String },
    /// A new tool-call approval was queued on the shared
    /// [`ApprovalQueue`]; the loop should refresh the approval UI.
    ApprovalRequested,
    /// An approval was resolved remotely (by another client); the loop
    /// should drop any pending UI entry for `call_id`.
    ApprovalResolved {
        call_id: String,
        decision: ApprovalDecision,
    },
}

/// Stream of [`TransportEvent`]s produced by a transport.
pub type TransportEventStream = Pin<Box<dyn Stream<Item = Result<TransportEvent>> + Send>>;

/// Bidirectional seam between the TUI event loop and whichever backend
/// the adapter talks to.
///
/// Implementations own:
/// * a way to push user messages out of the TUI (`submit`),
/// * a way to pull assistant/agent events into the TUI (`subscribe`),
/// * the local approval queue that the approval UI and the adapter's
///   `ChannelApprovalGate` both read/write.
#[async_trait]
pub trait TuiTransport: Send + Sync + 'static {
    /// Hand an incoming user message off to the backend.
    async fn submit(&self, msg: IncomingMessage) -> Result<()>;

    /// Open a long-lived subscription to backend events for
    /// `session_id`. The returned stream ends when the backend closes
    /// the connection.
    async fn subscribe(&self, session_id: &str) -> Result<TransportEventStream>;

    /// Shared approval queue. The TUI renders pending entries from it
    /// and the adapter's `ChannelApprovalGate` writes new requests
    /// into it; the transport additionally drives resolutions out
    /// of the queue back to the gateway.
    fn approval_queue(&self) -> ApprovalQueue;
}

/// Convenience alias so callers can store transports without rewriting
/// the trait-object form in every signature.
pub type SharedTransport = Arc<dyn TuiTransport>;
