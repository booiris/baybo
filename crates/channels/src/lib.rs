pub mod cli;
mod error;
mod registry;
mod slash;
mod types;

pub use cli::CliAdapter;
pub use error::ChannelError;
pub use registry::ChannelRegistry;
pub use slash::{SlashHandler, SlashOutcome};
pub use types::{ChannelStatus, IncomingMessage, Message, OutgoingMessage};

use async_trait::async_trait;
use aura_session::ChannelType;
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

    /// Gracefully shuts down the channel. Idempotent.
    async fn stop(&self) -> Result<()>;
}
