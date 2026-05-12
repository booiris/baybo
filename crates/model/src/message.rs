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
    /// `true` only when this message originated directly from a user
    /// channel input. The agent injects several `Role::User` messages
    /// of its own (skill reminders, system-reminders, etc.); this flag
    /// distinguishes the genuine prompt from those so trace replay can
    /// surface the user's actual input in the job summary panel.
    /// Defaults to `false` so existing call sites and pre-flag rows
    /// stay valid.
    #[serde(default, skip_serializing_if = "is_false")]
    pub from_user: bool,
}

fn is_false(v: &bool) -> bool {
    !*v
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
