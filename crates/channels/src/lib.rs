mod error;
mod registry;
mod slash;
mod types;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use error::ChannelError;
pub use registry::ChannelRegistry;
pub use slash::{
    DashboardProvider, DashboardSnapshot, SlashCommand, SlashHandler, SlashOutcome, ViewKind,
};
pub use types::{
    AgentOutput, ChannelStatus, IncomingMessage, Message, NoticeLevel, OutgoingMessage,
};

use std::sync::Arc;

use async_trait::async_trait;
use aura_model::ChannelType;
use aura_tools::ApprovalGate;
use tokio::sync::mpsc;

pub type Result<T> = std::result::Result<T, ChannelError>;

/// Unified adapter trait for all channel implementations.
///
/// Each channel converts platform-specific messages into `IncomingMessage`
/// and sends `OutgoingMessage` back in the platform-native format.
#[async_trait]
pub trait ChannelAdapter: Send + Sync + 'static {
    /// Returns the channel type identifier for this adapter.
    fn channel_type(&self) -> ChannelType;

    /// Starts listening for incoming messages in the background.
    ///
    /// Messages are pushed into the provided `sender`. The method returns
    /// once the background listener has been spawned.
    async fn start(&self, sender: mpsc::Sender<IncomingMessage>) -> Result<()>;

    /// Converts an outgoing message into the platform-native format and sends it.
    async fn send_response(&self, response: OutgoingMessage) -> Result<()>;

    /// Deliver an incremental text chunk for an in-flight assistant response.
    ///
    /// Channels that can render partial output (e.g. the TUI) accumulate
    /// chunks and redraw; channels without a partial surface (e.g. HTTP
    /// one-shot) return `Ok(())` without rendering. Every adapter must
    /// make the call explicitly — no default — so dropping deltas is a
    /// deliberate choice per channel, not a silent fallback.
    ///
    /// Delivery ordering per `session_id` is the caller's responsibility —
    /// channels assume chunks arrive in the order the LLM emitted them.
    async fn send_stream_delta(&self, session_id: &str, delta: &str) -> Result<()>;

    /// Deliver an out-of-band notice to the user for this session.
    ///
    /// Used for events the user didn't prompt for but should see (e.g.
    /// a skill they invoked was rated suspicious). Adapters without a
    /// surface for this must opt in to dropping it — no default impl,
    /// so the choice is always visible at the adapter site.
    async fn send_notice(&self, session_id: &str, level: NoticeLevel, text: &str) -> Result<()>;

    /// Return an approval gate for interactive tool-call approval.
    ///
    /// Channels that support an approval UX (e.g. TUI modal, Slack reaction)
    /// return `Some(gate)`; channels without one return `None`, which the
    /// registry treats as auto-deny. Called once at registration time.
    /// No default: forgetting to forward this from a wrapper silently
    /// converts all tool calls into auto-deny, so the choice must be
    /// explicit.
    fn approval_gate(&self) -> Option<Arc<dyn ApprovalGate>>;

    /// Gracefully shuts down the channel. Idempotent.
    async fn stop(&self) -> Result<()>;
}
