use aura_model::{ContentBlock, MessageMetadata};
use aura_session::{ChannelType, User};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub session_id: String,
    pub channel: ChannelType,
    pub sender: User,
    pub content: Vec<ContentBlock>,
    pub timestamp: DateTime<Utc>,
    pub reply_to: Option<String>,
    pub metadata: MessageMetadata,
}

/// Incoming message from a channel adapter, before security processing.
#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub message: Message,
}

/// Outgoing message to be sent back through a channel adapter.
#[derive(Debug, Clone)]
pub struct OutgoingMessage {
    pub session_id: String,
    pub channel: ChannelType,
    pub content: Vec<ContentBlock>,
    pub reply_to: Option<String>,
    pub metadata: MessageMetadata,
}

/// Lifecycle status of a registered channel adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelStatus {
    /// Registered but not yet started.
    Registered,
    /// Actively listening for messages.
    Running,
    /// Gracefully stopped.
    Stopped,
    /// Encountered an error during start or runtime.
    Error(String),
}
