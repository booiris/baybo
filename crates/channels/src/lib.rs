mod error;
mod registry;
mod slash;
mod types;
mod wire;

#[cfg(feature = "client")]
pub mod client;

#[cfg(feature = "sdk")]
pub mod sdk;

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
pub use wire::{ApprovalEvent, SseEvent};

use std::sync::Arc;

use async_trait::async_trait;
use aura_model::ChannelType;
use aura_tools::ApprovalGate;
use tokio::sync::mpsc;

pub type Result<T> = std::result::Result<T, ChannelError>;

/// Unified adapter trait for all channel implementations.
///
/// Each channel converts platform-specific messages into `IncomingMessage`
/// and sends `AgentOutput` back in the platform-native format.
#[async_trait]
pub trait ChannelAdapter: Send + Sync + 'static {
    /// Returns the channel type identifier for this adapter.
    fn channel_type(&self) -> ChannelType;

    /// Starts listening for incoming messages in the background.
    ///
    /// Messages are pushed into the provided `sender`. The method returns
    /// once the background listener has been spawned.
    async fn start(&self, sender: mpsc::Sender<IncomingMessage>) -> Result<()>;

    /// Deliver one agent output to the user-facing surface.
    ///
    /// Adapters dispatch on the `AgentOutput` variant themselves:
    /// - `Delta` is an incremental text chunk for an in-flight response.
    ///   Channels that can render partial output (e.g. the TUI) accumulate
    ///   chunks and redraw; channels without a partial surface return
    ///   `Ok(())` without rendering. Delivery ordering per `session_id` is
    ///   the caller's responsibility — channels assume chunks arrive in
    ///   the order the LLM emitted them.
    /// - `Message` is the final, canonical response for the turn.
    /// - `Notice` is an out-of-band notice the user didn't prompt for but
    ///   should see (e.g. a skill the user invoked was rated suspicious).
    ///   Adapters without a surface for this may drop it — the choice is
    ///   visible at the adapter's match arm.
    async fn send(&self, output: AgentOutput) -> Result<()>;

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
