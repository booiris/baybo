mod cli;
#[cfg(feature = "discord")]
mod discord;
#[cfg(feature = "discord")]
pub use discord::DiscordChannel;
mod http;
#[cfg(feature = "telegram")]
mod telegram;

pub use cli::CliChannel;
pub use http::HttpChannel;
#[cfg(feature = "telegram")]
pub use telegram::TelegramChannel;

use async_trait::async_trait;
use aura_core::{ChannelType, IncomingMessage, OutgoingMessage};
use tokio::sync::mpsc;

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
    async fn start(&self, sender: mpsc::Sender<IncomingMessage>) -> aura_core::Result<()>;

    /// Converts an outgoing message into the platform-native format and sends it.
    async fn send_response(&self, response: OutgoingMessage) -> aura_core::Result<()>;

    /// Gracefully shuts down the channel. Idempotent.
    async fn stop(&self) -> aura_core::Result<()>;
}
