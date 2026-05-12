use aura_model::{ChannelType, ResourceAccess, SessionId, User};
use aura_model::{ContentBlock, MessageMetadata};
use aura_tools::ApprovalDecision;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub session_id: SessionId,
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
    /// Client-supplied idempotency key carried over from
    /// [`crate::wire::Message::platform_msg_id`]. Echoed back unchanged
    /// in the [`SessionEvent::UserEcho`] fan-out so the sender's tab
    /// can reconcile its optimistic placeholder against the
    /// authoritative server-emitted row instead of double-rendering.
    /// Empty when the inbound carrier didn't set one (older web
    /// bundles, TUI, fixtures).
    pub platform_msg_id: String,
}

/// Outgoing message to be sent back through a channel adapter.
#[derive(Debug, Clone)]
pub struct OutgoingMessage {
    pub session_id: SessionId,
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

/// Role discriminator on a wire-shape [`crate::wire::Message`].
///
/// In-tree producers always set this explicitly: the agent emits
/// `Assistant`, sidecars and inbound echo emit `User`. Default is
/// `Assistant` so an old (pre-refactor) sidecar wire frame that omits
/// the field decodes as an agent reply — matches the historical
/// "outbound is always assistant" assumption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sdks/channel-ts/src/generated/")
)]
pub enum MessageRole {
    User,
    #[default]
    Assistant,
}

/// Things the agent itself emits while running a turn. Narrow on
/// purpose: producers in the agent crate can only ever construct one
/// of these three variants. Channel-side fan-out events (user echo,
/// approval prompts) live on [`SessionEvent`] instead, so the agent
/// can't accidentally synthesise a frame that's supposed to come from
/// the channel.
#[derive(Debug, Clone)]
pub enum AgentOutput {
    /// Incremental text chunk for the in-flight response on a session.
    Delta {
        session_id: SessionId,
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
        session_id: SessionId,
        /// Aura user id the notice is addressed to. See
        /// [`OutgoingMessage::user_id`].
        user_id: String,
        channel: ChannelType,
        level: NoticeLevel,
        text: String,
    },
}

impl AgentOutput {
    /// `session_id` this emission belongs to. Channel fan-out keys on
    /// this for selective-kind channels; broadcast channels still
    /// expose it for logging.
    pub fn session_id(&self) -> &SessionId {
        match self {
            AgentOutput::Delta { session_id, .. } => session_id,
            AgentOutput::Message(m) => &m.session_id,
            AgentOutput::Notice { session_id, .. } => session_id,
        }
    }

    /// Optional `user_id` addressee, when one is known. Empty string
    /// when the emission isn't user-addressed (cron / system).
    pub fn user_id(&self) -> &str {
        match self {
            AgentOutput::Delta { user_id, .. } => user_id,
            AgentOutput::Message(m) => &m.user_id,
            AgentOutput::Notice { user_id, .. } => user_id,
        }
    }
}

/// Everything fanned out to the connections subscribed to a
/// `session_id`. Wraps the agent's own [`AgentOutput`] alongside the
/// events the channel itself produces (inbound echo, approval-gate
/// prompts). The gateway's per-connection translator converts each
/// variant into the matching [`crate::wire::Frame`] before serialisation.
///
/// Splitting this from [`AgentOutput`] makes the producer/consumer
/// roles explicit: the agent router only ever hands us
/// `SessionEvent::Agent(_)` (statically); the channel hands us the
/// rest. Adding a new channel-side event no longer means widening the
/// agent's output surface.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Wraps an agent emission. Constructed via `From<AgentOutput>`.
    Agent(AgentOutput),
    /// Echo of an inbound user message back to every subscriber of the
    /// message's `session_id`. Lets multi-tab views render the user's
    /// own input through the same render path as agent output, so
    /// tab1 and tab2 of the same session converge on identical
    /// transcripts. The sender's tab receives it too; clients render
    /// straight from the echo rather than maintaining optimistic
    /// state.
    UserEcho(IncomingMessage),
    /// A tool call is blocked waiting for a human decision. Emitted by
    /// the channel's [`aura_tools::ApprovalGate`] waker so every
    /// subscriber to the call's `session_id` can show the approval UI
    /// concurrently; the first `ResolveApproval` wins, and the channel
    /// publishes [`SessionEvent::ApprovalResolved`] to dismiss the
    /// prompt elsewhere.
    ApprovalRequested {
        call_id: String,
        session_id: SessionId,
        user_id: String,
        tool: String,
        accesses: Vec<ResourceAccess>,
        params_preview: String,
        description: Option<String>,
    },
    /// Some subscriber resolved a pending approval; concurrent UIs
    /// drop the prompt. `session_id` is carried so the channel can
    /// fan out to the right subscriber set.
    ApprovalResolved {
        call_id: String,
        session_id: SessionId,
        decision: ApprovalDecision,
    },
}

impl From<AgentOutput> for SessionEvent {
    fn from(output: AgentOutput) -> Self {
        SessionEvent::Agent(output)
    }
}

impl SessionEvent {
    /// `session_id` this event belongs to. Channel fan-out keys on
    /// this for selective-kind channels; broadcast channels still
    /// expose it for logging.
    pub fn session_id(&self) -> &SessionId {
        match self {
            SessionEvent::Agent(out) => out.session_id(),
            SessionEvent::UserEcho(m) => &m.message.session_id,
            SessionEvent::ApprovalRequested { session_id, .. } => session_id,
            SessionEvent::ApprovalResolved { session_id, .. } => session_id,
        }
    }

    /// Optional `user_id` addressee, when one is known. Empty string
    /// when the event isn't user-addressed (cron / system / resolution
    /// notice).
    pub fn user_id(&self) -> &str {
        match self {
            SessionEvent::Agent(out) => out.user_id(),
            SessionEvent::UserEcho(m) => &m.message.sender.id,
            SessionEvent::ApprovalRequested { user_id, .. } => user_id,
            SessionEvent::ApprovalResolved { .. } => "",
        }
    }
}

/// Severity attached to an `AgentOutput::Notice`. Used only for
/// presentation — semantics (whether the action proceeded) are already
/// baked into the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    /// Action fully succeeded, surfaced as an out-of-band confirmation
    /// the LLM might not echo back into the conversation.
    Info,
    /// Action proceeded with a caveat worth seeing (e.g. suspicious
    /// skill still injected; profile write succeeded but git commit
    /// failed).
    Warn,
    /// Action was blocked or degraded (e.g. dangerous skill filtered
    /// out; tools the user asked for are unavailable this turn).
    Error,
}
