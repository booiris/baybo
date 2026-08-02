use serde::{Deserialize, Serialize};

use crate::approval::ApprovalDecision;
use crate::fingerprint::FileFingerprint;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContentBlock {
    Text(String),
    /// `filename` is the ORIGINAL name when one is known — an agent-attached
    /// image, or an inbound one whose channel supplied a name. It is `Option`
    /// because plenty of images genuinely arrive nameless (a pasted screenshot,
    /// a Telegram photo, an MCP-returned bitmap), not to accommodate callers.
    /// Nothing renders it beside the picture; it exists so a client SHARING or
    /// saving the image hands over the real name instead of `attachment.png`.
    /// `#[serde(default)]` keeps transcripts persisted before this field readable.
    Image {
        blob: BlobRef,
        mime_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
        /// Pixel width and height, the only honest price input the context
        /// budget has for an image. Providers cut an image into tiles of a
        /// fixed PIXEL size and bill per tile, so byte count is not a
        /// stand-in: a 12000x9000 flat render sits under the 5 MiB payload
        /// cap and costs 49,536 tokens where a 4096-px ceiling charged
        /// 9,288.
        ///
        /// **Server-derived, never client-declared**, from
        /// [`baybo_llm::media_probe::image_dimensions`] — a header parse,
        /// not a decode. `None` for a vector image, a raster format the
        /// probe cannot read, or a row persisted before the fields existed;
        /// the budget then charges the delivery cap, which the delivery
        /// path's own probe makes a real ceiling by stubbing anything
        /// pricier.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<u32>,
    },
    Audio {
        blob: BlobRef,
        mime_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
        /// Playback length in milliseconds, probed from the container at
        /// attach time (`AttachFile`). `None` when unknown — inbound channel
        /// audio, unparseable containers, rows persisted before the field
        /// existed. Carried so a client can show a track's length before any
        /// byte is downloaded or played.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u32>,
    },
    File {
        blob: BlobRef,
        filename: String,
        mime_type: String,
        /// Playback length in milliseconds for VIDEO files (video rides the
        /// `File` kind — it has no block variant of its own), probed from the
        /// container at attach time. Same contract as `Audio::duration_ms`:
        /// `None` when unknown or not a video.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u32>,
        /// Pages, for a PDF the model may receive as a native document.
        /// Providers bill such a document per PAGE, so this is the only
        /// honest price input the context budget has — byte count is not a
        /// stand-in for it (measured across three producers, real documents
        /// run 10 to 4,007 bytes per page).
        ///
        /// **Server-derived, never client-declared.** Set by whoever ingests
        /// the blob, from the bytes, via
        /// [`baybo_llm::media_probe::pdf_page_count`]. `None` when the file
        /// is not a PDF, when it did not parse, or for rows persisted before
        /// the field existed — the budget then charges the delivery cap
        /// rather than guessing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        page_count: Option<u32>,
        /// Byte length of the blob, for a file the model receives as
        /// inlined prompt text. Those bytes ARE the prompt, so this is the
        /// price input for that arm the way `page_count` is for a PDF —
        /// without it the budget can only charge the delivery cap, and a
        /// 400-byte `.md` was billed 17,529 tokens.
        ///
        /// **Server-derived, never client-declared**, from
        /// [`crate::BlobRef`]'s stored metadata rather than from anything
        /// on the wire. `None` for rows persisted before the field existed
        /// or when the blob could not be stat'd — the budget then falls
        /// back to the delivery cap.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size_bytes: Option<u32>,
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
        /// Side-band metadata persisted with the transcript but **never sent to
        /// the LLM** (the provider conversion emits only `content`). `None` for
        /// the vast majority of results and for legacy rows; see
        /// [`ToolResultMeta`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<ToolResultMeta>,
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

/// Side-band metadata for a [`ContentBlock::ToolResult`]: data the runtime
/// needs to persist with the transcript but that must stay off the universal
/// `content` string (so it never reaches the LLM). A typed extension point —
/// new per-result metadata adds a field here rather than re-touching every
/// `ToolResult` match site.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolResultMeta {
    /// Fingerprint of the file a `Read` returned. On session hydration the
    /// read-before-write tracker is rebuilt from these, so a `Read` in a
    /// pre-restart turn still satisfies an `Edit` afterwards. `None` for every
    /// non-`Read` tool result. See [`FileFingerprint`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_fingerprint: Option<FileFingerprint>,
    /// Fingerprint this call **left on disk**, set only when an `Edit`/`Write`
    /// actually rewrote the file.
    ///
    /// Deliberately not [`Self::read_fingerprint`], which those tools also
    /// carry: that one is the anchor the call *vouched for*, and a write that
    /// failed, errored, or was denied still has one — the pre-call value — so a
    /// reader asking "did this conversation leave these bytes" would answer yes
    /// for an edit that never landed. `None` here means this call changed
    /// nothing, which is the question `baybo-context` asks before it tells the
    /// model a persona file still says what the model wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_fingerprint: Option<FileFingerprint>,
    /// Decision the user's approval gate returned for this call — the durable
    /// record behind the transcript's "approved / always / denied" step label.
    /// `None` for calls that never raised a prompt (covered by prior grants or
    /// an ungated tool) and for rows persisted before the field existed. When
    /// a call prompts more than once (mid-call `ApprovalHandle` asks), the
    /// last decision wins, which matches the call's final status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalDecision>,
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

impl BlobRef {
    /// See [`blob_content_digest`].
    pub fn content_digest(&self) -> Option<&str> {
        blob_content_digest(&self.blob_id)
    }
}

/// Algorithm prefix on every [`BlobRef::blob_id`]. The full id is
/// `"sha256:<64 lower-hex>.<read-token>"`, so a content-addressed blob is
/// recognizable at a glance while the suffix stays the read capability. Lives in
/// `model` (next to [`BlobRef`]) so both the server (`baybo-store`, which mints
/// ids) and the device client (which parses them) reference one source of truth.
pub const SHA256_PREFIX: &str = "sha256:";

/// The content-addressed half of a `blob_id` — the SHA-256 hex, without the
/// algorithm prefix or the trailing read token.
///
/// Every `BlobStore::put` mints a **fresh** read token, so two writes of
/// identical bytes yield two distinct `blob_id`s that share this digest. The
/// digest, not the id, is a blob's content identity; comparing ids to answer
/// "is this the same file?" silently always says no. `None` for a malformed id.
pub fn blob_content_digest(blob_id: &str) -> Option<&str> {
    blob_id
        .strip_prefix(SHA256_PREFIX)?
        .split('.')
        .next()
        .filter(|hex| !hex.is_empty())
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
    /// A cron job's fire-time framed prompt: synthesized by the agent and
    /// hidden from the chat transcript, but tracked distinctly so operator
    /// surfaces identify a fire by provenance rather than by sniffing the
    /// framing tag out of the content.
    Cron,
    /// A one-shot cron job's result, delivered into the conversation that
    /// created the turn. A `Role::Assistant` row the agent appended **without
    /// running inference** (see the actor's `CronResultReady` handler): the
    /// fire ran in its own isolated session and this row carries its reply
    /// framed with a scheduled-task header. The only assistant-role source
    /// other than [`MessageSource::Agent`] — chat surfaces render it as an
    /// assistant bubble with a scheduled-task badge.
    CronNotification,
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
    /// A notice that the sources behind the leading `Role::System` row have
    /// moved on since it was written, carrying the parts that now differ. The
    /// system row is durable and a session outlives the deploy that seeded it,
    /// so a persona-path migration or an edited `SOUL.md` would otherwise leave
    /// the model working from paths that no longer exist until the next
    /// compaction reseeds. Rides as a framed `Role::User` row — never
    /// `Role::System`, which providers accept only in the leading slot — and is
    /// tracked distinctly so the reconciler can find what it has already said
    /// by provenance instead of sniffing its envelope tag out of the content.
    SystemPromptUpdate,
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
            MessageSource::CronNotification => "cron_notification",
            MessageSource::RecalledMemory => "recalled_memory",
            MessageSource::SystemPromptUpdate => "system_prompt_update",
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
            "cron_notification" => Ok(MessageSource::CronNotification),
            "recalled_memory" => Ok(MessageSource::RecalledMemory),
            "system_prompt_update" => Ok(MessageSource::SystemPromptUpdate),
            "agent" => Ok(MessageSource::Agent),
            other => Err(format!("unknown message source: {other}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    /// Client-supplied idempotency key for genuine channel input. Persisted so
    /// reconnect/history replay can reconcile optimistic user bubbles.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    platform_msg_id: String,
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
            platform_msg_id: String::new(),
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
            platform_msg_id: String::new(),
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
            platform_msg_id: String::new(),
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
            platform_msg_id: String::new(),
            source: MessageSource::RecalledMemory,
        }
    }

    /// A one-shot cron fire's result, appended to the conversation that created
    /// the turn — the **only** constructor that marks a row
    /// [`MessageSource::CronNotification`].
    ///
    /// `Role::Assistant`, but produced with no inference of its own: the actor's
    /// `CronResultReady` handler builds `content` by framing the isolated fire
    /// session's reply (see `baybo_context::prompts::cron::frame_cron_notification`)
    /// and appends it at a turn boundary. The framing lives in the persisted
    /// content, so user, model, and boot-time replay all read the same bytes.
    pub fn cron_notification(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::Assistant,
            content,
            platform_msg_id: String::new(),
            source: MessageSource::CronNotification,
        }
    }

    /// A notice that the leading `Role::System` row no longer matches the
    /// sources it was assembled from, carrying the parts that differ — the
    /// **only** constructor that marks a row [`MessageSource::SystemPromptUpdate`].
    ///
    /// `Role::User` rather than `Role::System` for the same reason
    /// [`Self::recalled_memory`] is: a second system row is rejected outside the
    /// leading slot by providers that validate placement. The reconciler
    /// (`baybo_context::ContextManager::reconcile_system_prompt`) reads back the
    /// rows it has already appended to decide what is genuinely new, so the
    /// content must stay exactly what was sent — no wire-only re-framing.
    pub fn system_prompt_update(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::User,
            content,
            platform_msg_id: String::new(),
            source: MessageSource::SystemPromptUpdate,
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
            platform_msg_id: String::new(),
            source: MessageSource::Agent,
        }
    }

    /// An assistant turn (model output: text, tool calls, thinking blocks).
    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::Assistant,
            content,
            platform_msg_id: String::new(),
            source: MessageSource::Agent,
        }
    }

    /// A leading/system-prompt row.
    pub fn system(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::System,
            content,
            platform_msg_id: String::new(),
            source: MessageSource::Agent,
        }
    }

    /// A `Role::Tool` row carrying tool output back to the model.
    pub fn tool(content: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::Tool,
            content,
            platform_msg_id: String::new(),
            source: MessageSource::Agent,
        }
    }

    /// Convenience for the common single-[`ContentBlock::ToolResult`] row.
    pub fn tool_result(tool_use_id: String, content: String) -> Self {
        Self::tool(vec![ContentBlock::ToolResult {
            tool_use_id,
            content,
            meta: None,
        }])
    }

    /// Like [`Self::tool_result`] but carries [`ToolResultMeta`] (an empty/`None`
    /// meta collapses to [`Self::tool_result`]). The agent loop uses this for
    /// `Read` results so the read-before-write tracker can be rebuilt on
    /// hydration; the metadata is persisted but not LLM-visible.
    pub fn tool_result_with_meta(
        tool_use_id: String,
        content: String,
        meta: Option<ToolResultMeta>,
    ) -> Self {
        Self::tool(vec![ContentBlock::ToolResult {
            tool_use_id,
            content,
            meta,
        }])
    }

    /// This row's provenance — see [`MessageSource`]. Operator surfaces use it
    /// to tell a genuine prompt, a cron fire, and agent-injected context apart
    /// even though all three are `Role::User`.
    pub fn source(&self) -> MessageSource {
        self.source
    }

    pub fn with_platform_msg_id(mut self, platform_msg_id: impl Into<String>) -> Self {
        self.platform_msg_id = platform_msg_id.into();
        self
    }

    pub fn platform_msg_id(&self) -> &str {
        &self.platform_msg_id
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

    /// `true` when this row's prose belongs in the full-text index: everything
    /// a human authored or an agent said, across every session — a subagent's
    /// own run and a cron fire's are as searchable as a chat. Scope is the
    /// caller's turn, off `message_fts.channel`; see `docs/search.md`.
    ///
    /// What stays out is text nobody composed as a turn: the system prompt and
    /// skill reminders and `<system-reminder>` blocks
    /// ([`MessageSource::Agent`] on a non-assistant role), recalled memories
    /// ([`MessageSource::RecalledMemory`], whose text the memory backend already
    /// owns, so indexing it stores the same prose twice), and tool output
    /// (`Role::Tool`, which cannot be ranked beside prose — a tool row runs five
    /// orders of magnitude longer than a typed question and `bm25` weights
    /// columns, not rows).
    ///
    /// Neither field decides this alone: [`MessageSource::Agent`] covers both
    /// assistant output (a real turn) and the system prompt (not one), and
    /// `Role::User` covers both a typed prompt and an injected reminder.
    pub fn is_searchable(&self) -> bool {
        self.from_user() || self.role == Role::Assistant || self.source == MessageSource::Cron
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
    /// `#[serde(rename_all = "snake_case")]` form so JSON, sqlite
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
    use std::time::{Duration, SystemTime};

    #[test]
    fn identical_bytes_staged_twice_share_a_digest_but_not_an_id() {
        // The shape `BlobStore::put` mints: same content, fresh read token.
        let hex = "a".repeat(64);
        let first = BlobRef {
            blob_id: format!("{SHA256_PREFIX}{hex}.tok-one"),
        };
        let second = BlobRef {
            blob_id: format!("{SHA256_PREFIX}{hex}.tok-two"),
        };
        assert_ne!(first.blob_id, second.blob_id);
        assert_eq!(first.content_digest(), Some(hex.as_str()));
        assert_eq!(first.content_digest(), second.content_digest());
    }

    #[test]
    fn a_malformed_blob_id_has_no_digest() {
        for id in ["", "sha256:", "md5:abcdef", "abcdef", "sha256:.tok"] {
            assert_eq!(blob_content_digest(id), None, "id={id:?}");
        }
    }

    #[test]
    fn a_tokenless_id_is_all_digest() {
        let hex = "b".repeat(64);
        assert_eq!(
            blob_content_digest(&format!("{SHA256_PREFIX}{hex}")),
            Some(hex.as_str())
        );
    }

    #[test]
    fn tool_result_meta_round_trips() {
        let fp = FileFingerprint::new(SystemTime::UNIX_EPOCH + Duration::from_secs(99), 1234);
        let block = ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: "body".into(),
            meta: Some(ToolResultMeta {
                read_fingerprint: Some(fp),
                write_fingerprint: Some(fp),
                approval: Some(ApprovalDecision::ApproveAlways),
            }),
        };
        let json = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, back);
    }

    #[test]
    fn tool_result_without_meta_omits_the_key() {
        // `None` must not serialize the key, so existing rows stay byte-stable
        // and the field is invisible wherever the block is rendered.
        let json = serde_json::to_string(&ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: "body".into(),
            meta: None,
        })
        .unwrap();
        assert!(!json.contains("meta"), "{json}");
    }

    #[test]
    fn legacy_tool_result_without_field_deserializes_to_none() {
        // A row persisted before the field existed must still load.
        let legacy = r#"{"ToolResult":{"tool_use_id":"t1","content":"body"}}"#;
        let block: ContentBlock = serde_json::from_str(legacy).unwrap();
        match block {
            ContentBlock::ToolResult { meta, .. } => assert!(meta.is_none()),
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    /// A `File` row written before the price inputs existed is the common case
    /// in any live transcript, and the budget reads both fields on every count
    /// — so the stored form has to keep loading, and a `None` has to stay off
    /// the wire so already-written rows stay byte-stable.
    #[test]
    fn legacy_file_block_without_price_inputs_deserializes_to_none() {
        let legacy = r#"{"File":{"blob":{"blob_id":"sha256:aa.tok"},"filename":"report.pdf","mime_type":"application/pdf"}}"#;
        let block: ContentBlock = serde_json::from_str(legacy).unwrap();
        match &block {
            ContentBlock::File {
                page_count,
                size_bytes,
                duration_ms,
                filename,
                ..
            } => {
                assert_eq!(*page_count, None);
                assert_eq!(*size_bytes, None);
                assert_eq!(*duration_ms, None);
                assert_eq!(filename, "report.pdf");
            }
            other => panic!("expected File, got {other:?}"),
        }
        assert_eq!(serde_json::to_string(&block).unwrap(), legacy);
    }

    #[test]
    fn probed_price_inputs_round_trip() {
        let block = ContentBlock::File {
            blob: BlobRef {
                blob_id: "sha256:aa.tok".into(),
            },
            filename: "report.pdf".into(),
            mime_type: "application/pdf".into(),
            duration_ms: None,
            page_count: Some(12),
            size_bytes: Some(4_096),
        };
        let json = serde_json::to_string(&block).unwrap();
        assert_eq!(serde_json::from_str::<ContentBlock>(&json).unwrap(), block);
    }

    /// Every `MessageSource` variant. The exhaustive `match` makes adding a
    /// variant a compile error here until it's listed below — which forces both
    /// the round-trip test and the TS-mirror test to account for the new variant.
    fn all_message_sources() -> Vec<MessageSource> {
        fn _exhaustive(s: MessageSource) {
            match s {
                MessageSource::User
                | MessageSource::UserInterjection
                | MessageSource::Cron
                | MessageSource::CronNotification
                | MessageSource::RecalledMemory
                | MessageSource::SystemPromptUpdate
                | MessageSource::Agent => {}
            }
        }
        vec![
            MessageSource::User,
            MessageSource::UserInterjection,
            MessageSource::Cron,
            MessageSource::CronNotification,
            MessageSource::RecalledMemory,
            MessageSource::SystemPromptUpdate,
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

    /// The hand-maintained TS mirror `app/web/src/types/trace.ts` is **not** covered
    /// by `scripts/check-ts-bindings.sh` (that gate only spans the ts-rs
    /// surfaces), so it has silently drifted before. This guards it directly: the
    /// `MessageSource` union there must list exactly the serialized form of every
    /// Rust variant — catching both a new Rust variant the mirror forgot and a
    /// stale member the mirror kept.
    #[test]
    fn message_source_matches_ts_mirror() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../app/web/src/types/trace.ts"
        );
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
            "MessageSource drift between baybo_model and app/web/src/types/trace.ts — \
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
