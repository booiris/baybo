use aura_model::{ChannelType, User};
use aura_model::{ContentBlock, MessageMetadata};
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

/// Protocol between agent actors and the router.
///
/// Actors emit streaming deltas as they receive text from the LLM, then a
/// final `Message` once the full turn is assembled. The router forwards
/// each variant to the appropriate [`crate::Channel::send`] site —
/// channels without a partial surface ignore deltas and only act on the
/// final message.
#[derive(Debug, Clone)]
pub enum AgentOutput {
    /// Incremental text chunk for the in-flight response on a session.
    Delta {
        session_id: String,
        channel: ChannelType,
        text: String,
    },
    /// Final, canonical assistant response for the turn.
    Message(OutgoingMessage),
    /// Out-of-band notice addressed to the user for a specific session.
    ///
    /// Distinct from `Message` because the user did not prompt for it —
    /// it's emitted by the agent when an operational condition warrants
    /// surfacing (e.g. a skill the user invoked has been rated
    /// `Suspicious`, so it still runs but the user should know).
    /// Channels decide how to render; the TUI inlines it into scrollback
    /// styled by `level`, transports without a banner surface may drop it.
    Notice {
        session_id: String,
        channel: ChannelType,
        level: NoticeLevel,
        text: String,
    },
}

/// Severity attached to an `AgentOutput::Notice`. Used only for
/// presentation — semantics (whether the action proceeded) are already
/// baked into the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    /// Action proceeded with a caveat worth seeing (e.g. suspicious
    /// skill still injected).
    Warn,
    /// Action was blocked or degraded (e.g. dangerous skill filtered
    /// out; tools the user asked for are unavailable this turn).
    Error,
}
