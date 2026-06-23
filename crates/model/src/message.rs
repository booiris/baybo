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

/// Open-tag prefix of the `<tool_output name="...">` envelope the agent wraps
/// untrusted tool results in before they enter the LLM transcript. Shared so
/// the wrapper (`baybo-context`) and the injection detector's forged-delimiter
/// rule (`baybo-security`) can never disagree on the literal they key off.
pub const TOOL_OUTPUT_OPEN_PREFIX: &str = "<tool_output";

/// Close-tag prefix (`</tool_output`, without the trailing `>`): the wrapper
/// neutralises any literal occurrence inside the body so untrusted content
/// can't forge a boundary back to instructions, and the injection detector
/// flags forged ones in untrusted input.
pub const TOOL_OUTPUT_CLOSE_PREFIX: &str = "</tool_output";

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
    /// A genuine human channel message that arrived **mid-turn**, while the
    /// agent loop was still working on the user's previous request. Renders as
    /// a user bubble just like [`MessageSource::User`] (see
    /// [`ChatMessage::from_user`]), but is tracked distinctly so the wire layer
    /// can frame it with a `<user_interjection>` steering envelope
    /// (`baybo_context::prompts::interjection`). See
    /// `docs/mid-turn-user-interjection.md`.
    UserInterjection,
    /// A cron job's fire-time framed prompt: synthesized by the agent, so
    /// hidden from the chat transcript, but surfaced on its own in the
    /// operator cron inbox (which finds it by this variant rather than by
    /// sniffing the framing tag out of the content).
    Cron,
    /// A block of memories recalled from long-term storage and injected to
    /// inform the current turn. Synthesized by the agent (so hidden from the
    /// chat transcript like [`MessageSource::Agent`]), but tracked distinctly
    /// so the wire layer frames it with a `<recalled_memory>` steering envelope
    /// (`baybo_context::prompts::recalled_memory`) and operator surfaces can tell
    /// recalled context apart from a genuine turn. Always rides as a framed
    /// `Role::User` row — never `Role::System`, which would re-assert itself on
    /// every later turn (the failure mode that retired the prior memory
    /// pipeline).
    RecalledMemory,
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
            MessageSource::UserInterjection => "user_interjection",
            MessageSource::Cron => "cron",
            MessageSource::RecalledMemory => "recalled_memory",
            MessageSource::Agent => "agent",
        }
    }
}

impl std::str::FromStr for MessageSource {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "user" => Ok(MessageSource::User),
            "user_interjection" => Ok(MessageSource::UserInterjection),
            "cron" => Ok(MessageSource::Cron),
            "recalled_memory" => Ok(MessageSource::RecalledMemory),
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

    /// A genuine human channel message that arrived **mid-turn**, while the
    /// agent loop was still working on the user's previous request. Like
    /// [`Self::user`] it carries human-authored content and renders as a user
    /// bubble, but it is stamped [`MessageSource::UserInterjection`] so the wire
    /// layer frames it with a `<user_interjection>` steering envelope. The
    /// **only** constructor that marks a row [`MessageSource::UserInterjection`].
    pub fn user_interjection(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::User,
            content,
            source: MessageSource::UserInterjection,
        }
    }

    /// A cron job's fire-time framed prompt — a `Role::User` turn the agent
    /// synthesized at fire time (see the agent crate's
    /// `baybo_context::prompts::cron::frame_cron_prompt`). Carries [`MessageSource::Cron`] so
    /// the operator cron inbox can locate it by provenance instead of sniffing
    /// the `[cron:<id>]` framing tag, while the chat transcript still hides it.
    pub fn cron_fire(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::User,
            content,
            source: MessageSource::Cron,
        }
    }

    /// A block of memories recalled from long-term storage, injected as a framed
    /// `Role::User` row to inform the current turn — the **only** constructor
    /// that marks a row [`MessageSource::RecalledMemory`]. Never `Role::System`
    /// (which would pollute every later turn); the wire layer wraps it in a
    /// `<recalled_memory>` envelope (`baybo_context::prompts::recalled_memory`)
    /// re-derived per call. Carries [`MessageSource::RecalledMemory`] so it never
    /// surfaces as a user bubble.
    pub fn recalled_memory(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::User,
            content,
            source: MessageSource::RecalledMemory,
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

    /// `true` when this row came directly from a human channel input — a
    /// genuine prompt ([`MessageSource::User`]) or a mid-turn interjection
    /// ([`MessageSource::UserInterjection`]); see [`ChatMessage::user`] /
    /// [`ChatMessage::user_interjection`]. Chat surfaces use it to surface
    /// human-authored turns and hide agent-injected `Role::User` rows. Note
    /// this is broader than "is a genuine top-level prompt" — slash-command
    /// detection keys off `source() == MessageSource::User` exactly, since an
    /// interjection is never a slash command.
    pub fn from_user(&self) -> bool {
        matches!(
            self.source,
            MessageSource::User | MessageSource::UserInterjection
        )
    }

    /// True when this message carries any [`ContentBlock::ToolUse`]. For an
    /// assistant turn that marks it as an *intermediate* agentic iteration
    /// (it issued tool calls, so the loop continued) rather than the final
    /// reply — the chat surface treats such a turn's narration as live-only
    /// work progress, not a durable answer bubble.
    pub fn has_tool_use(&self) -> bool {
        self.content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Every `MessageSource` variant. The exhaustive `match` makes adding a
    /// variant a compile error here until it's listed below — which forces both
    /// the round-trip test and the TS-mirror test to account for the new variant.
    fn all_message_sources() -> Vec<MessageSource> {
        fn _exhaustive(s: MessageSource) {
            match s {
                MessageSource::User
                | MessageSource::UserInterjection
                | MessageSource::Cron
                | MessageSource::RecalledMemory
                | MessageSource::Agent => {}
            }
        }
        vec![
            MessageSource::User,
            MessageSource::UserInterjection,
            MessageSource::Cron,
            MessageSource::RecalledMemory,
            MessageSource::Agent,
        ]
    }

    #[test]
    fn message_source_string_round_trips() {
        for src in all_message_sources() {
            assert_eq!(src.as_str().parse::<MessageSource>(), Ok(src));
        }
        assert_eq!(
            MessageSource::UserInterjection.as_str(),
            "user_interjection"
        );
    }

    /// The hand-maintained TS mirror `web/src/types/trace.ts` is **not** covered
    /// by `scripts/check-ts-bindings.sh` (that gate only spans the ts-rs
    /// surfaces), so it has silently drifted before. This guards it directly: the
    /// `MessageSource` union there must list exactly the serialized form of every
    /// Rust variant — catching both a new Rust variant the mirror forgot and a
    /// stale member the mirror kept.
    #[test]
    fn message_source_matches_ts_mirror() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../web/src/types/trace.ts");
        let src = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));

        // Slice the `export type MessageSource = ...;` declaration (robust to the
        // union spanning multiple lines) and pull its single-quoted members.
        let start = src
            .find("export type MessageSource")
            .unwrap_or_else(|| panic!("`export type MessageSource` not found in {path}"));
        let decl = &src[start..];
        let end = decl
            .find(';')
            .unwrap_or_else(|| panic!("MessageSource union not `;`-terminated in {path}"));
        let ts_members: BTreeSet<String> = decl[..end]
            .split('\'')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect();

        let rust_members: BTreeSet<String> = all_message_sources()
            .iter()
            .map(|s| s.as_str().to_string())
            .collect();

        assert_eq!(
            rust_members, ts_members,
            "MessageSource drift between baybo_model and web/src/types/trace.ts — \
             keep the TS union in sync with the Rust enum"
        );
    }

    #[test]
    fn user_interjection_is_a_user_bubble_role_and_source() {
        let m = ChatMessage::user_interjection(vec![ContentBlock::Text("hi".into())]);
        assert_eq!(m.role, Role::User);
        assert_eq!(m.source(), MessageSource::UserInterjection);
        // Renders as a user bubble (broadened from_user), unlike agent-injected
        // Role::User rows.
        assert!(m.from_user());
        assert!(!ChatMessage::agent_context(vec![ContentBlock::Text("x".into())]).from_user());
    }

    #[test]
    fn recalled_memory_is_hidden_user_role_with_distinct_source() {
        let m = ChatMessage::recalled_memory(vec![ContentBlock::Text("user prefers Rust".into())]);
        assert_eq!(m.role, Role::User);
        assert_eq!(m.source(), MessageSource::RecalledMemory);
        // Injected context, not human input: must NOT render as a user bubble
        // and must never be treated as a genuine prompt / slash command.
        assert!(!m.from_user());
    }
}
