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
    /// Persisted `session_messages.ordinal` of this assistant row.
    /// `Some` when the agent loop captured it from the storage append
    /// before dispatching; gateway adapters stamp it onto
    /// `Frame::Message.ordinal` so reconnecting web/TUI clients
    /// advance their catch-up cursor past this row. `None` only when
    /// persistence failed (logged) or the producer doesn't go through
    /// the persisted-message path (cron stubs, test fixtures).
    pub ordinal: Option<i64>,
}

/// Role discriminator on a wire-shape [`crate::wire::Message`].
///
/// In-tree producers always set this explicitly: the agent emits
/// `Assistant`, sidecars and inbound echo emit `User`. Default is
/// `Assistant` so a wire frame that omits the field decodes as an
/// agent reply.
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

/// Something the agent emits about an in-flight turn, addressed to one
/// session. The addressing triple (`session_id` / `user_id` / `channel`)
/// is hoisted into this envelope so every [`AgentEvent`] variant shares
/// it without repetition; `event` carries what actually happened.
///
/// Narrow on purpose: producers in the agent crate construct these.
/// Channel-side fan-out events (user echo, approval prompts) live on
/// [`SessionEvent`] instead, so the agent can't accidentally synthesise
/// a frame that's supposed to come from the channel.
#[derive(Debug, Clone)]
pub struct AgentOutput {
    /// Session this emission belongs to. Channel fan-out keys on this
    /// for selective-kind channels; broadcast channels still expose it
    /// for logging.
    pub session_id: SessionId,
    /// Aura user id this emission is addressed to. See
    /// [`OutgoingMessage::user_id`] for the per-channel routing
    /// rationale. Empty string when not user-addressed (cron / system).
    pub user_id: String,
    pub channel: ChannelType,
    pub event: AgentEvent,
}

/// What the agent emitted, minus the addressing carried by the
/// enclosing [`AgentOutput`].
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Incremental answer-prose chunk for the in-flight response — the
    /// reply text, distinct from the `Reasoning` thinking trace.
    AnswerDelta(String),
    /// Incremental model reasoning ("thinking") chunk. Rendered
    /// dim/collapsible by channels that support it; never persisted as
    /// answer content. Goes through the same sanitize/reveal boundary as
    /// `AnswerDelta`.
    Reasoning(String),
    /// A tool call has started. `label` is the human preview
    /// (`Tool::call_label`), falling back to the tool name client-side;
    /// `call_id` pairs this with the matching [`AgentEvent::ToolCompleted`].
    ToolStarted {
        call_id: String,
        tool: String,
        label: Option<String>,
    },
    /// A tool call finished. `summary` is a short, presentation-only
    /// rendering of the result ("Read 200 lines", "exit 0", "Error: …").
    ToolCompleted {
        call_id: String,
        status: ToolStatus,
        summary: String,
    },
    /// Coarse turn-phase transition for a transient status line (today:
    /// context compaction start/end). Channels show a spinner/banner and
    /// clear it on the matching end; surfaces without one drop it.
    Status(TurnStatus),
    /// Final, canonical assistant response for the turn.
    Message(OutgoingMessage),
    /// Out-of-band notice addressed to the user.
    ///
    /// Distinct from `Message` because the user did not prompt for it —
    /// it's emitted by the agent when an operational condition warrants
    /// surfacing (e.g. a skill the user invoked has been rated
    /// `Suspicious`, so it still runs but the user should know).
    /// Channels decide how to render; the TUI inlines it into scrollback
    /// styled by `level`, transports without a banner surface may drop it.
    Notice { level: NoticeLevel, text: String },
}

/// Outcome of a finished tool call, carried by
/// [`AgentEvent::ToolCompleted`]. Presentation-only — flattened to a
/// lower-case string on the wire (`Frame::ToolCompleted`), mirroring how
/// [`NoticeLevel`] becomes `Notice.level`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    /// Tool returned normally.
    Ok,
    /// Tool errored (`ToolOutput::Error`, or the executor itself failed).
    Error,
    /// The user denied the call at the approval gate.
    Denied,
}

/// Coarse turn-phase signal carried by [`AgentEvent::Status`]. Today only
/// context compaction reports start/end; future phases (Thinking,
/// Responding, …) can extend this. Presentation-only — flattened to a
/// lower-case string on the wire (`Frame::Status.phase`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    /// Context compaction (summarisation) has started — the turn paused
    /// to shrink the transcript before the next LLM call.
    Compacting,
    /// Context compaction finished; the turn resumes.
    Compacted,
}

impl From<OutgoingMessage> for AgentOutput {
    /// Wrap a final response into an output addressed to its own
    /// session — the envelope's addressing triple mirrors the message's.
    fn from(m: OutgoingMessage) -> Self {
        Self {
            session_id: m.session_id.clone(),
            user_id: m.user_id.clone(),
            channel: m.channel.clone(),
            event: AgentEvent::Message(m),
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
            SessionEvent::Agent(out) => &out.session_id,
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
            SessionEvent::Agent(out) => &out.user_id,
            SessionEvent::UserEcho(m) => &m.message.sender.id,
            SessionEvent::ApprovalRequested { user_id, .. } => user_id,
            SessionEvent::ApprovalResolved { .. } => "",
        }
    }
}

/// Severity attached to an `AgentEvent::Notice`. Used only for
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
