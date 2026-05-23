use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContentBlock {
    Text(String),
    Image {
        blob: BlobRef,
        mime_type: String,
    },
    Audio {
        blob: BlobRef,
        mime_type: String,
    },
    File {
        blob: BlobRef,
        filename: String,
        mime_type: String,
    },
    /// A tool invocation emitted by the assistant. Stored in conversation
    /// history so subsequent LLM calls see their own prior tool calls.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        /// Provider-specific cryptographic signature (e.g. Gemini's
        /// `thought_signature`). Must be echoed back verbatim.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        signature: Option<String>,
    },
    /// The result of a tool invocation, keyed back to the originating
    /// [`ToolUse`] via `tool_use_id`.
    ToolResult {
        tool_use_id: String,
        content: String,
    },
    /// Thinking/reasoning blocks emitted by the model. Must be preserved
    /// and echoed back for providers that require it (Anthropic extended
    /// thinking, Gemini thought signatures).
    Thinking {
        #[serde(skip_serializing_if = "Option::is_none", default)]
        id: Option<String>,
        content: Vec<ThinkingContent>,
    },
}

/// A single thinking/reasoning content item from the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ThinkingContent {
    /// Main thinking text with optional cryptographic signature.
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        signature: Option<String>,
    },
    /// A provider summary of thinking.
    Summary { text: String },
    /// Opaque encrypted or redacted reasoning that must be echoed verbatim.
    Redacted { data: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    pub blob_id: String,
}

/// Where a [`ChatMessage`] row came from — its provenance, independent of the
/// LLM-facing [`Role`]. Several distinct origins all ride as a `Role::User`
/// turn (a human's input, a cron fire's framed prompt, an agent-injected skill
/// reminder), so role alone can't tell them apart; operator surfaces key off
/// this instead of guessing by content. Sealed behind the [`ChatMessage`]
/// constructors — set once, never flipped by a raw literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageSource {
    /// A genuine message a human sent through a channel input. The only source
    /// that renders as a user bubble on chat surfaces.
    User,
    /// A cron job's fire-time framed prompt: synthesized by the agent, so
    /// hidden from the chat transcript, but surfaced on its own in the
    /// operator cron inbox (which finds it by this variant rather than by
    /// sniffing the framing tag out of the content).
    Cron,
    /// Any other agent-originated row: skill reminders, a spawned/subagent task
    /// prompt, the subagent-finished notification, summary instructions, the
    /// system prompt, assistant output, tool results. Hidden from chat surfaces.
    Agent,
}

impl MessageSource {
    /// Canonical lowercase wire/db spelling, matching the
    /// `#[serde(rename_all = "snake_case")]` form.
    pub fn as_str(&self) -> &'static str {
        match self {
            MessageSource::User => "user",
            MessageSource::Cron => "cron",
            MessageSource::Agent => "agent",
        }
    }
}

impl std::str::FromStr for MessageSource {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "user" => Ok(MessageSource::User),
            "cron" => Ok(MessageSource::Cron),
            "agent" => Ok(MessageSource::Agent),
            other => Err(format!("unknown message source: {other}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    /// Where this row came from. Sealed (non-`pub`) so provenance is set once,
    /// at the typed constructor, never flipped by a raw literal:
    /// [`ChatMessage::user`] is the sole producer of [`MessageSource::User`],
    /// [`ChatMessage::cron_fire`] the sole producer of [`MessageSource::Cron`],
    /// and every other constructor stamps [`MessageSource::Agent`]. Read via
    /// [`ChatMessage::source`] (or the [`ChatMessage::from_user`] convenience).
    source: MessageSource,
}

impl ChatMessage {
    /// A genuine message a human sent through a channel input — the **only**
    /// constructor that marks a row [`MessageSource::User`].
    ///
    /// Every synthetic `Role::User` row the agent injects — a skill reminder, a
    /// spawned/subagent task prompt, the subagent-finished notification — must
    /// use [`ChatMessage::agent_context`] instead (and a cron fire uses
    /// [`ChatMessage::cron_fire`]), so chat surfaces never present synthesized
    /// content as something the user typed.
    pub fn user(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::User,
            content,
            source: MessageSource::User,
        }
    }

    /// A cron job's fire-time framed prompt — a `Role::User` turn the agent
    /// synthesized at fire time (see the agent crate's
    /// `cron_prompt::frame_cron_prompt`). Carries [`MessageSource::Cron`] so
    /// the operator cron inbox can locate it by provenance instead of sniffing
    /// the `[cron:<id>]` framing tag, while the chat transcript still hides it.
    pub fn cron_fire(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::User,
            content,
            source: MessageSource::Cron,
        }
    }

    /// An agent-injected `Role::User` row: content the model should read as a
    /// user turn (a skill reminder, a spawned subagent's task, the
    /// subagent-finished notification) but that no human sent. Carries
    /// [`MessageSource::Agent`], so it never surfaces as a user bubble.
    pub fn agent_context(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::User,
            content,
            source: MessageSource::Agent,
        }
    }

    /// An assistant turn (model output: text, tool calls, thinking blocks).
    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::Assistant,
            content,
            source: MessageSource::Agent,
        }
    }

    /// A leading/system-prompt row.
    pub fn system(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::System,
            content,
            source: MessageSource::Agent,
        }
    }

    /// A `Role::Tool` row carrying tool output back to the model.
    pub fn tool(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::Tool,
            content,
            source: MessageSource::Agent,
        }
    }

    /// Convenience for the common single-[`ContentBlock::ToolResult`] row.
    pub fn tool_result(tool_use_id: String, content: String) -> Self {
        Self::tool(vec![ContentBlock::ToolResult {
            tool_use_id,
            content,
        }])
    }

    /// This row's provenance — see [`MessageSource`]. Operator surfaces use it
    /// to tell a genuine prompt, a cron fire, and agent-injected context apart
    /// even though all three are `Role::User`.
    pub fn source(&self) -> MessageSource {
        self.source
    }

    /// `true` only when this row came directly from a human channel input
    /// (i.e. [`MessageSource::User`]; see [`ChatMessage::user`]). Chat surfaces
    /// use it to surface the genuine prompt and hide agent-injected
    /// `Role::User` rows.
    pub fn from_user(&self) -> bool {
        matches!(self.source, MessageSource::User)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    /// Canonical lowercase wire/db spelling. Matches the
    /// `#[serde(rename_all = "snake_case")]` form so JSON, libsql
    /// rows, and any other string-shaped channel agree.
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

impl std::str::FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "system" => Ok(Role::System),
            "user" => Ok(Role::User),
            "assistant" => Ok(Role::Assistant),
            "tool" => Ok(Role::Tool),
            other => Err(format!("unknown role: {other}")),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageMetadata {}
