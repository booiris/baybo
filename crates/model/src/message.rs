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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    /// `true` only when this row originated directly from a human channel
    /// input. Sealed (non-`pub`) so provenance is set once, at the typed
    /// constructor, never flipped by a raw literal: [`ChatMessage::user`]
    /// is the sole producer of `true`; every synthetic `Role::User` row
    /// goes through [`ChatMessage::agent_context`] (`false`). Read via
    /// [`ChatMessage::from_user`].
    from_user: bool,
}

impl ChatMessage {
    /// A genuine message a human sent through a channel input — the **only**
    /// constructor that marks a row user-authored (`from_user = true`).
    ///
    /// Every synthetic `Role::User` row the agent injects — a cron fire's
    /// framed prompt, a spawned/subagent task prompt, a skill reminder, the
    /// subagent-finished notification — must use [`ChatMessage::agent_context`]
    /// instead, so chat surfaces never present synthesized content as
    /// something the user typed.
    pub fn user(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::User,
            content,
            from_user: true,
        }
    }

    /// An agent-injected `Role::User` row: content the model should read as a
    /// user turn (cron-fire framing, a spawned subagent's task, a skill
    /// reminder, the subagent-finished notification) but that no human sent.
    /// Carries `from_user = false`, so it never surfaces as a user bubble.
    pub fn agent_context(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::User,
            content,
            from_user: false,
        }
    }

    /// An assistant turn (model output: text, tool calls, thinking blocks).
    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::Assistant,
            content,
            from_user: false,
        }
    }

    /// A leading/system-prompt row.
    pub fn system(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::System,
            content,
            from_user: false,
        }
    }

    /// A `Role::Tool` row carrying tool output back to the model.
    pub fn tool(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::Tool,
            content,
            from_user: false,
        }
    }

    /// Convenience for the common single-[`ContentBlock::ToolResult`] row.
    pub fn tool_result(tool_use_id: String, content: String) -> Self {
        Self::tool(vec![ContentBlock::ToolResult {
            tool_use_id,
            content,
        }])
    }

    /// `true` only when this row came directly from a human channel input
    /// (see [`ChatMessage::user`]). Chat surfaces use it to surface the
    /// genuine prompt and hide agent-injected `Role::User` rows.
    pub fn from_user(&self) -> bool {
        self.from_user
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
