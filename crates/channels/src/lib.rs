mod error;
mod registry;
mod slash;
mod tui;
mod types;

pub use error::ChannelError;
pub use registry::ChannelRegistry;
pub use slash::{
    DashboardProvider, DashboardSnapshot, SlashCommand, SlashHandler, SlashOutcome, ViewKind,
};
pub use tui::event::{LogLevel, LogRecord, TuiLogSink};
pub use tui::{OnExit, TuiAdapter};
pub use types::{
    AgentOutput, ChannelStatus, IncomingMessage, Message, NoticeLevel, OutgoingMessage,
};

use async_trait::async_trait;
use aura_model::ChannelType;
use tokio::sync::mpsc;

pub type Result<T> = std::result::Result<T, ChannelError>;

/// Unified adapter trait for all channel implementations.
///
/// Each channel converts platform-specific messages into `IncomingMessage`
/// and sends `OutgoingMessage` back in the platform-native format.
///
/// Concrete implementations live outside this crate (in `channels/`) and
/// are loaded at runtime via the WASM extension mechanism.
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
    /// one-shot) may drop deltas and render only the final `send_response`.
    /// The default implementation is a no-op for that reason.
    ///
    /// Delivery ordering per `session_id` is the caller's responsibility —
    /// channels assume chunks arrive in the order the LLM emitted them.
    async fn send_stream_delta(&self, session_id: &str, delta: &str) -> Result<()> {
        let _ = (session_id, delta);
        Ok(())
    }

    /// Deliver an out-of-band notice to the user for this session.
    ///
    /// Used for events the user didn't prompt for but should see (e.g.
    /// a skill they invoked was rated suspicious). Channels that have
    /// no user-visible surface for such notices may drop them; the
    /// default implementation does exactly that.
    async fn send_notice(&self, session_id: &str, level: NoticeLevel, text: &str) -> Result<()> {
        let _ = (session_id, level, text);
        Ok(())
    }

    /// Gracefully shuts down the channel. Idempotent.
    async fn stop(&self) -> Result<()>;
}
