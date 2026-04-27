use aura_model::{ChannelType, User};
use aura_model::{ContentBlock, MessageMetadata};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    /// Aura user id this response is addressed to — the same value the
    /// inbound `Message.sender.id` carried. Channel adapters that route
    /// replies by user (e.g. the Telegram sidecar keying `user_id` to
    /// `chat_id`) consume this instead of reverse-mapping from
    /// `session_id`. Empty string when the message isn't user-addressed
    /// (e.g. cron ticks emitted outside a live conversation).
    pub user_id: String,
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
        /// Aura user id the in-flight response is addressed to. See
        /// [`OutgoingMessage::user_id`] for the per-channel routing
        /// rationale. Empty string when not user-addressed.
        user_id: String,
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
        /// Aura user id the notice is addressed to. See
        /// [`OutgoingMessage::user_id`].
        user_id: String,
        channel: ChannelType,
        level: NoticeLevel,
        text: String,
    },
    /// Structured progress event for an in-flight agent loop iteration.
    ///
    /// Channels that don't render fine-grained progress (e.g. a stock
    /// chat sidecar) drop these without rendering. UIs that do (the
    /// TUI, the WebUI's session inspector, future Slack threads) use
    /// `kind` to render a step indicator and `summary` as the human
    /// label. `structured` carries kind-specific extras the renderer
    /// may surface (token counts, latency, etc.) without locking the
    /// schema across the whole codebase.
    Progress {
        session_id: String,
        user_id: String,
        channel: ChannelType,
        job_id: String,
        span_id: String,
        span_index: u32,
        kind: ProgressKind,
        summary: String,
        structured: Option<Value>,
    },
}

/// One discrete step in the agent's progress through an iteration.
///
/// Variants are added as the surrounding system grows (sub-agent and
/// acceptance flows pull in more); sidecars that decode an unknown
/// variant should drop the event silently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProgressKind {
    /// A new ReAct iteration just opened.
    SpanStarted,
    /// The iteration's LLM picked a tool and the executor is dispatching it.
    ToolStarted { tool_name: String },
    /// A tool dispatched by the iteration finished, success or not.
    ToolCompleted { tool_name: String, ok: bool },
    /// The iteration body finished (final response, max-iter exit, or error).
    SpanCompleted,
    /// JobManager moved the job from `Completed` to `Submitted`.
    JobSubmitted,
    /// JobManager moved the job from `Submitted` to `Accepted`.
    JobAccepted,
    /// A sub-agent was spawned in a child session; parent waits for it.
    SubAgentSpawned { child_session_id: String },
    /// The sub-agent's job reached a terminal state.
    SubAgentCompleted { child_session_id: String, ok: bool },
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
