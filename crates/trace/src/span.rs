//! `Span` — one atomic action (LLM call or tool call) with a start /
//! end window and a closed strong-typed `kind`.
//!
//! Spans are direct children of `Step`s. Spans within a step may run in
//! parallel (sibling spans sharing a `parallel_group: ParallelGroup`)
//! but never nest. LLM ↔ tool pairing is by `ToolCallOrigin`, not by
//! tree structure.

use baybo_model::{ChatMessage, ContentBlock, ParallelGroup, SpanId, StepId, ToolSetHash};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use serde_json::Value;

use crate::event::SpanEvent;
use crate::outcome::{LifecycleOutcome, LifecycleState};

/// One atomic action recorded in the trace tree.
///
/// **Lifecycle invariant:** `outcome.is_terminal() ⟺ ended_at.is_some()`.
/// Mutate the two together via [`Span::close`]; never set one without
/// the other. Recovery, storage rewrites, and replay all rely on this
/// pairing.
///
/// **Storage coupling:** the `spans` table in `baybo-storage` derives its
/// indexed `step_id` / `started_at` columns from this struct's serialized
/// JSON via `json_extract(data, '$.step_id')` (and `'$.started_at'`) —
/// those field names are load-bearing for `list_spans_by_step` and ordering.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub id: SpanId,
    pub step_id: StepId,
    pub kind: SpanKind,

    /// Spans sharing a `parallel_group` ran concurrently — their time
    /// windows may overlap. `None` means strictly sequential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel_group: Option<ParallelGroup>,

    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,

    /// Lifecycle state. `Pending` until the span ends.
    pub outcome: LifecycleState,

    /// Sub-events (sanitize hits / approvals).
    /// In storage these live in their own `span_events` table keyed by
    /// `(span_id, seq)`. `load_span` does NOT join — fetch via
    /// `list_span_events(span_id)` when this field needs to be populated
    /// (see `agent::query::QueryApi::load_step` / `replay`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<SpanEvent>,
}

impl Span {
    /// Atomically transition this span to a terminal outcome. Sets
    /// both `outcome` and `ended_at` from the single call so the
    /// lifecycle invariant cannot be violated. The terminal-only
    /// [`LifecycleOutcome`] type makes "close with `Pending`"
    /// unrepresentable.
    pub fn close(&mut self, outcome: LifecycleOutcome, at: DateTime<Utc>) {
        self.outcome = LifecycleState::Done(outcome);
        self.ended_at = Some(at);
    }
}

/// Closed enum of every kind of atomic action. Each variant splits its
/// data into begin-time provenance (`begin`) and end-time result
/// (`result`, `Option` because `Pending` / cancelled spans never see
/// one written). No superset struct, no `serde_json::Value` payload as
/// a backdoor — except `ToolCallBegin.params` / `ToolCallResult.output`
/// where the schema is genuinely dynamic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpanKind {
    LlmCall {
        begin: LlmCallBegin,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<LlmCallResult>,
    },
    ToolCall {
        begin: ToolCallBegin,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<ToolCallResult>,
    },
}

impl SpanKind {
    pub fn tag(&self) -> &'static str {
        match self {
            SpanKind::LlmCall { .. } => "llm_call",
            SpanKind::ToolCall { .. } => "tool_call",
        }
    }
}

/// Begin-time data for an `LlmCall` span — set when the request is
/// dispatched, never mutated afterwards.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmCallBegin {
    pub model_id: String,
    pub provider: String,
    /// What the LLM saw on this call. The two variants split based on
    /// whether the input is in the per-session append-only log:
    ///
    /// - Main agent calls reference `session_messages` by ordinal,
    ///   keeping span storage constant per call instead of cloning a
    ///   growing prefix every turn (the prior shape was O(N²) over
    ///   session length).
    /// - One-off calls whose input never lands in the session log
    ///   (compression summarisations, subagent briefings) embed
    ///   their messages inline because there's nothing to point to.
    pub input_messages: LlmCallInputs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// The thinking level this call actually ran at, on baybo's ladder:
    /// the session's pin when set, else the entry's configured level, else
    /// whatever the provider resolves for itself. `None` when the provider's
    /// dialect carries no effort at all, and on spans recorded before the
    /// field existed. Same spelling as `cost_records.reasoning_effort`, so
    /// a span and its spend row agree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// The tool set this call offered the model, as a reference into the
    /// content-addressed `llm_tool_sets` table. `None` when the call
    /// offered no tools (compression, title generation, the progress
    /// observer) and on spans recorded before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<LlmToolSetRef>,
}

/// Reference from an `LlmCall` span to the tool set it offered.
///
/// A reference rather than the definitions themselves for the same reason
/// [`LlmCallInputs::Persisted`] exists: the set is session-stable and runs
/// to tens of KB, so an inline copy on every call would be the dominant
/// term in the `spans` table. `count` rides along so a reader can see how
/// many tools were on offer without resolving the set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmToolSetRef {
    pub hash: ToolSetHash,
    pub count: usize,
}

/// One tool as the model was shown it. Mirrors the provider-facing
/// definition rather than the internal `Tool` trait — the trace records
/// what went out on the wire.
///
/// `parameters_schema` is a `Value` for the same reason
/// `ToolCallBegin.params` is: tool schemas are genuinely dynamic. That is
/// the documented boundary, not a new one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,
}

/// The full set of definitions one [`LlmToolSetRef`] points at — the
/// entity stored in `llm_tool_sets`, keyed by the hash of its own
/// canonical serialization.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmToolSet {
    pub tools: Vec<LlmToolDefinition>,
}

impl LlmToolSet {
    pub fn new(tools: Vec<LlmToolDefinition>) -> Self {
        Self { tools }
    }
}

/// Source of the `input_messages` for an `LlmCall` span.
///
/// `Persisted` keeps the span small by referencing a snapshot of the
/// session message log; the gateway rehydrates this back into a flat
/// `Vec<ChatMessage>` for clients that want to inspect the exact slice
/// the LLM saw.
///
/// **Wire shape**: `#[serde(untagged)]` so `Inline` rides as a bare
/// array (matching the long-standing `input_messages: ChatMessage[]`
/// on-the-wire shape consumed by the web UI) and `Persisted` rides as
/// a struct. Variant order matters here — array-first means the
/// deserialiser tries `Inline` before `Persisted`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LlmCallInputs {
    /// Messages embedded directly. Used when the input is not — and
    /// should not be — part of the session message log: compression
    /// LLM calls, subagent briefings, etc. Also the post-hydration
    /// shape every consumer downstream of `QueryApi::replay` sees.
    Inline(Vec<ChatMessage>),
    /// Active set of `session_messages` as of `last_ordinal`. The
    /// hydrated slice is recovered with the standard "active as of
    /// ordinal X" filter:
    /// `WHERE ordinal <= last_ordinal AND
    ///        (superseded_by IS NULL OR superseded_by > last_ordinal)`.
    /// System messages live in `session_messages` like any other row,
    /// so hydration restores the leading `Role::System` (when present)
    /// directly from the same query — no separate join.
    Persisted {
        /// Highest `session_messages.ordinal` that was active at call
        /// time. The active set as of this ordinal is the slice the
        /// LLM saw.
        last_ordinal: i64,
        /// Self-validating tripwire: the number of prefix messages the
        /// writer expected hydration to reconstruct from the log (the
        /// active-as-of-`last_ordinal` count, suffix excluded). Hydration
        /// compares the reconstructed count against this; a mismatch means
        /// the log drifted under the reference (a `superseded_by` bug, a
        /// deleted row, or a read/write filter divergence) and is
        /// surfaced LOUDLY instead of returning a plausible-but-wrong
        /// slice. Always written — a `Persisted` marker is only emitted
        /// when the count is known, so every reference is validated.
        /// See `docs/modules/trace.md`.
        prefix_len: usize,
        /// Exact ordinal order for a repaired transcript window. Empty means
        /// the ordinary active-as-of slice above. A non-empty list lets the
        /// writer omit quarantined orphan/duplicate results and reposition a
        /// valid result beside its `ToolUse` without cloning every message
        /// into the span. Hydration verifies that every ordinal belongs to the
        /// active-as-of set, then emits rows in this order.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        ordinals: Vec<i64>,
        /// Framing messages appended after the persisted active set
        /// that are NOT themselves rows in `session_messages` — the
        /// compression `SUMMARIZE_INSTRUCTION`, the progress-observer
        /// prompt, and (for the background-summary tool loop) the
        /// sub-loop's own assistant / tool turns. Empty for main-agent
        /// calls, which see only the persisted prefix. Hydration appends
        /// these verbatim after the reconstructed active slice, so the
        /// rebuilt `Inline` view equals the exact `request.messages` the
        /// LLM saw. Kept inline because there is nothing in the log to
        /// point at; tiny next to the prefix it replaces.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        suffix: Vec<ChatMessage>,
    },
}

impl LlmCallInputs {
    /// Construct an empty inline payload — used as a placeholder when
    /// deserialising span rows whose body lacks an `input_messages`
    /// field (e.g. crash-recovered placeholders).
    pub fn empty() -> Self {
        Self::Inline(Vec::new())
    }

    /// True when this call's input is recoverable from
    /// `session_messages` rather than embedded inline.
    pub fn is_persisted(&self) -> bool {
        matches!(self, Self::Persisted { .. })
    }
}

/// End-time result for an `LlmCall` span — set when the response is
/// received. `None` while the span is `Pending` or if it cancelled
/// before producing a result.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LlmCallResult {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output_content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<LlmToolCallRecord>,
    /// The WHOLE prompt, cache included — `baybo_llm::TokenUsage` normalises
    /// every provider to that meaning before this is recorded, so the two
    /// cache fields below are a BREAKDOWN of this number, never addends. A
    /// consumer that adds them back double-counts every cache hit.
    #[serde(default)]
    pub input_tokens: usize,
    #[serde(default)]
    pub output_tokens: usize,
    /// Prompt-cache: how much of `input_tokens` was served from the cache.
    /// `#[serde(default)]` keeps already-persisted spans (which lack
    /// the field) decodable.
    #[serde(default)]
    pub cached_input_tokens: usize,
    /// Prompt-cache: how much of `input_tokens` was written into the cache.
    #[serde(default)]
    pub cache_creation_input_tokens: usize,
}

/// Begin-time data for a `ToolCall` span.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallBegin {
    pub tool_name: String,
    /// Pairing back to the LLM `Span` that emitted the tool_use block.
    /// Currently always `Some` — every tool call goes through the agent
    /// loop. The field stays optional for storage backwards compat with
    /// historical rows from removed direct-invoke paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triggered_by: Option<ToolCallOrigin>,
    pub params: Value,
}

/// End-time result for a `ToolCall` span.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallResult {
    /// The tool's result. Larger values point at the durable transcript row
    /// that already carries the capped model-facing payload; smaller values
    /// stay inline.
    #[serde(default)]
    pub output: ToolCallOutput,
    #[serde(default)]
    pub success: bool,
    /// Serialized byte length of the untruncated output, set only when the
    /// model-facing payload was capped. `None` means the resolved result is
    /// complete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_truncated_from: Option<usize>,
}

/// A tool result recorded directly in the span, or a pointer to the durable
/// transcript row that already carries the same model-facing payload.
///
/// The `Inline(Value)` wire shape is unchanged for historical rows and for
/// small results. `Persisted` serializes as a marker object so a larger result
/// can point at its `session_messages` row (by `tool_use_id`) instead of
/// cloning the same payload into both tables.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolCallOutput {
    Persisted(PersistedToolCallOutput),
    Inline(Value),
}

impl Default for ToolCallOutput {
    fn default() -> Self {
        Self::Inline(Value::Null)
    }
}

impl ToolCallOutput {
    pub fn inline(value: Value) -> Self {
        Self::Inline(value)
    }

    pub fn persisted(reference: PersistedToolCallOutput) -> Self {
        Self::Persisted(reference)
    }

    pub fn is_persisted(&self) -> bool {
        matches!(self, Self::Persisted(_))
    }

    /// Resolve a transcript-backed output into an inline JSON value. Inline
    /// values pass through unchanged.
    pub fn resolve(&self, message: &ChatMessage) -> std::result::Result<Value, String> {
        match self {
            Self::Inline(value) => Ok(value.clone()),
            Self::Persisted(reference) => reference.resolve(message),
        }
    }
}

const SESSION_TOOL_RESULT_REFERENCE_TAG: &str = "session_tool_result";

/// Typed serde sentinel that makes the untagged persisted-reference object
/// unambiguous with ordinary dynamic tool JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SessionToolResultReferenceTag;

impl Serialize for SessionToolResultReferenceTag {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(SESSION_TOOL_RESULT_REFERENCE_TAG)
    }
}

impl<'de> Deserialize<'de> for SessionToolResultReferenceTag {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value == SESSION_TOOL_RESULT_REFERENCE_TAG {
            Ok(Self)
        } else {
            Err(D::Error::custom(
                "expected $baybo_ref to be session_tool_result",
            ))
        }
    }
}

/// Pointer to the `ToolResult` the model saw, keyed by the `tool_use_id` the
/// LLM assigned the call. Resolving finds that block in the session log and
/// returns its exact model-facing content (the wrapped, capped envelope) — no
/// second copy is stored in the span. `tool_use_id` is a begin-time fact, so
/// the span can close during execution without waiting for the transcript
/// append; a rare append failure leaves the pointer unresolvable (surfaced as a
/// `trace_reconstruction_error` on read) rather than dangling silently.
///
/// `attachments` / `llm_images` are the media blocks a `WithAttachments` /
/// `MultiModalText` result carries out of band of its text — they never enter
/// the `ToolResult` content, so the reference keeps them (small `BlobRef`
/// pointers) to preserve the trace panel's blob list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedToolCallOutput {
    #[serde(rename = "$baybo_ref")]
    reference_tag: SessionToolResultReferenceTag,
    pub tool_use_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub llm_images: Vec<ContentBlock>,
}

impl PersistedToolCallOutput {
    pub fn new(
        tool_use_id: String,
        attachments: Vec<ContentBlock>,
        llm_images: Vec<ContentBlock>,
    ) -> Self {
        Self {
            reference_tag: SessionToolResultReferenceTag,
            tool_use_id,
            attachments,
            llm_images,
        }
    }

    /// Resolve against the `message` that carries this call's `ToolResult`.
    /// Text/JSON/error results return the raw model-facing content string;
    /// media results wrap it back into the historical tagged object so the
    /// out-of-band `attachments` / `llm_images` blob list survives.
    pub fn resolve(&self, message: &ChatMessage) -> std::result::Result<Value, String> {
        let content = message
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if tool_use_id == &self.tool_use_id => Some(content.as_str()),
                _ => None,
            })
            .ok_or_else(|| format!("session log has no ToolResult for {}", self.tool_use_id))?;
        if !self.attachments.is_empty() {
            Ok(serde_json::json!({
                "type": "with_attachments",
                "text": content,
                "attachments": self.attachments,
            }))
        } else if !self.llm_images.is_empty() {
            Ok(serde_json::json!({
                "type": "multi_modal_text",
                "text": content,
                "llm_images": self.llm_images,
            }))
        } else {
            Ok(Value::String(content.to_string()))
        }
    }
}

/// One tool_use block emitted by an LLM. Recorded inside the LLM
/// span's `result.tool_calls` so the next iteration's tool spans can
/// pair back via `ToolCallOrigin`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmToolCallRecord {
    /// Provider's `tool_use_id` (whatever the API uses to pair the
    /// `tool_use` block with its eventual `tool_result`).
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// Pointer back to the LLM span that requested this tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallOrigin {
    pub llm_span_id: SpanId,
    pub tool_use_id: String,
}

/// Opaque handle returned by `SpanRecorder::begin_span`. Carries the
/// begin-time `kind`, `started_at`, and `parallel_group` so the recorder
/// can persist the closed span without re-loading.
#[derive(Debug, Clone, PartialEq)]
pub struct SpanHandle {
    pub span_id: SpanId,
    pub step_id: StepId,
    pub kind: SpanKind,
    pub started_at: DateTime<Utc>,
    pub parallel_group: Option<ParallelGroup>,
}

impl SpanHandle {
    pub fn new(
        span_id: SpanId,
        step_id: StepId,
        kind: SpanKind,
        started_at: DateTime<Utc>,
        parallel_group: Option<ParallelGroup>,
    ) -> Self {
        Self {
            span_id,
            step_id,
            kind,
            started_at,
            parallel_group,
        }
    }
}

/// End-time additions handed to `SpanRecorder::end_span`. Variants line
/// up 1:1 with `SpanKind` value-bearing variants (`SubagentStub` and
/// any cancel path use [`SpanFinalize::Empty`]). Mismatched variants
/// at end-time are a programming error and surface as
/// `TraceError::Internal`.
#[derive(Debug, Clone, PartialEq)]
pub enum SpanFinalize {
    LlmCall(LlmCallResult),
    ToolCall(ToolCallResult),
    /// No additional end-time data. Used for `SubagentStub` spans (the
    /// `child_session_id` was set at begin) and for any cancel path
    /// that closes without a meaningful result.
    Empty,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_llm() -> SpanKind {
        SpanKind::LlmCall {
            begin: LlmCallBegin {
                model_id: "claude-sonnet-4-6".into(),
                provider: "anthropic".into(),
                reasoning_effort: Some("high".into()),
                input_messages: LlmCallInputs::empty(),
                temperature: Some(0.7),
                tools: None,
            },
            result: None,
        }
    }

    fn dummy_tool() -> SpanKind {
        SpanKind::ToolCall {
            begin: ToolCallBegin {
                tool_name: "bash".into(),
                triggered_by: Some(ToolCallOrigin {
                    llm_span_id: SpanId::new(),
                    tool_use_id: "tu-1".into(),
                }),
                params: serde_json::json!({"cmd": "ls"}),
            },
            result: None,
        }
    }

    fn pending_span(kind: SpanKind) -> Span {
        Span {
            id: SpanId::new(),
            step_id: StepId::new(),
            kind,
            parallel_group: None,
            started_at: Utc::now(),
            ended_at: None,
            outcome: LifecycleState::Pending,
            events: vec![],
        }
    }

    #[test]
    fn close_pairs_outcome_and_ended_at() {
        let mut span = pending_span(dummy_llm());
        let now = Utc::now();
        span.close(LifecycleOutcome::Ok, now);
        assert_eq!(span.outcome, LifecycleState::Done(LifecycleOutcome::Ok));
        assert_eq!(span.ended_at, Some(now));
    }

    #[test]
    fn span_round_trips_through_serde() {
        let span = Span {
            id: SpanId::new(),
            step_id: StepId::new(),
            kind: dummy_llm(),
            parallel_group: None,
            started_at: Utc::now(),
            ended_at: None,
            outcome: LifecycleState::Pending,
            events: vec![],
        };
        let s = serde_json::to_string(&span).unwrap();
        let back: Span = serde_json::from_str(&s).unwrap();
        assert_eq!(back, span);
    }

    #[test]
    fn tool_span_with_parallel_group_round_trips() {
        let pg = ParallelGroup::new();
        let kind = dummy_tool();
        let span = Span {
            id: SpanId::new(),
            step_id: StepId::new(),
            kind: kind.clone(),
            parallel_group: Some(pg),
            started_at: Utc::now(),
            ended_at: Some(Utc::now()),
            outcome: LifecycleState::Done(LifecycleOutcome::Ok),
            events: vec![],
        };
        let s = serde_json::to_string(&span).unwrap();
        let back: Span = serde_json::from_str(&s).unwrap();
        assert_eq!(back.parallel_group, Some(pg));
        assert_eq!(back.kind, kind);
    }

    #[test]
    fn span_handle_is_constructible() {
        let h = SpanHandle::new(SpanId::new(), StepId::new(), dummy_llm(), Utc::now(), None);
        assert_eq!(h.span_id, h.clone().span_id);
    }

    /// Lock the on-the-wire JSON shape of `LlmCallInputs` so a future
    /// refactor can't accidentally re-introduce the tagged-enum form
    /// the web UI doesn't grok. `Inline` rides as a bare array (the
    /// long-standing `input_messages: ChatMessage[]` shape); only
    /// `Persisted` is an object.
    #[test]
    fn llm_call_inputs_serializes_inline_as_bare_array() {
        let inline = LlmCallInputs::Inline(vec![baybo_model::ChatMessage::agent_context(vec![
            baybo_model::ContentBlock::Text("hi".into()),
        ])]);
        let json = serde_json::to_value(&inline).unwrap();
        assert!(json.is_array(), "Inline must serialize as a bare array");
        assert_eq!(json.as_array().unwrap().len(), 1);

        let persisted = LlmCallInputs::Persisted {
            last_ordinal: 7,
            prefix_len: 3,
            ordinals: vec![],
            suffix: vec![],
        };
        let json = serde_json::to_value(&persisted).unwrap();
        assert!(json.is_object(), "Persisted must serialize as an object");
        assert_eq!(json["last_ordinal"], 7);
        assert_eq!(json["prefix_len"], 3);
        assert!(
            json.get("ordinals").is_none(),
            "healthy transcript markers must not pay for an empty projection"
        );
        assert!(
            json.get("suffix").is_none(),
            "an empty suffix must be skipped so main-agent spans stay wire-compact"
        );

        // Round-trip both shapes back through Deserialize.
        let v1: LlmCallInputs = serde_json::from_value(serde_json::json!([])).unwrap();
        assert!(matches!(v1, LlmCallInputs::Inline(_)));
        // The minimal Persisted shape (no suffix key) decodes with the
        // suffix defaulting to empty; `prefix_len` is required.
        let v2: LlmCallInputs =
            serde_json::from_value(serde_json::json!({"last_ordinal": 1, "prefix_len": 2}))
                .unwrap();
        assert!(matches!(
            v2,
            LlmCallInputs::Persisted {
                last_ordinal: 1,
                prefix_len: 2,
                ref ordinals,
                ref suffix,
            } if ordinals.is_empty() && suffix.is_empty()
        ));
    }

    /// A repaired ordinal projection and non-empty suffix survive a serde
    /// round-trip and stay distinguishable from bare-array `Inline`.
    #[test]
    fn persisted_projection_and_suffix_round_trip() {
        let suffix = vec![baybo_model::ChatMessage::agent_context(vec![
            baybo_model::ContentBlock::Text("SUMMARIZE NOW".into()),
        ])];
        let persisted = LlmCallInputs::Persisted {
            last_ordinal: 42,
            prefix_len: 2,
            ordinals: vec![7, 3],
            suffix: suffix.clone(),
        };
        let json = serde_json::to_value(&persisted).unwrap();
        assert!(json.is_object());
        assert_eq!(json["last_ordinal"], 42);
        assert_eq!(json["ordinals"], serde_json::json!([7, 3]));
        assert!(json["suffix"].is_array());

        let back: LlmCallInputs = serde_json::from_value(json).unwrap();
        assert_eq!(
            back,
            LlmCallInputs::Persisted {
                last_ordinal: 42,
                prefix_len: 2,
                ordinals: vec![7, 3],
                suffix,
            }
        );
    }

    #[test]
    fn tool_call_output_keeps_legacy_inline_wire_shape() {
        let legacy = serde_json::json!({"type": "text", "text": "hello"});
        let output: ToolCallOutput = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(output, ToolCallOutput::Inline(legacy.clone()));
        assert_eq!(serde_json::to_value(output).unwrap(), legacy);
    }

    #[test]
    fn persisted_tool_output_requires_the_exact_reference_tag() {
        let ordinary_json = serde_json::json!({
            "$baybo_ref": "some_other_reference",
            "tool_use_id": "tool-9",
        });
        let output: ToolCallOutput = serde_json::from_value(ordinary_json.clone()).unwrap();
        assert_eq!(output, ToolCallOutput::Inline(ordinary_json));
    }

    #[test]
    fn persisted_tool_output_round_trips_and_resolves_to_the_wrapped_content() {
        let wrapped = "<tool_output name=\"Read\">\nbody </tool_output> text\n</tool_output>";
        let reference = PersistedToolCallOutput::new("tool-9".into(), vec![], vec![]);
        let output = ToolCallOutput::Persisted(reference);
        let wire = serde_json::to_value(&output).unwrap();
        assert_eq!(wire["$baybo_ref"], "session_tool_result");
        let back: ToolCallOutput = serde_json::from_value(wire).unwrap();
        let message = ChatMessage::tool_result("tool-9".into(), wrapped.to_string());
        assert_eq!(
            back.resolve(&message).unwrap(),
            serde_json::Value::String(wrapped.to_string())
        );
    }

    #[test]
    fn persisted_tool_output_keeps_media_blocks_as_a_tagged_object() {
        let image = baybo_model::ContentBlock::Text("blob-ref-stand-in".into());
        let reference = PersistedToolCallOutput::new("tool-9".into(), vec![image.clone()], vec![]);
        let message = ChatMessage::tool_result("tool-9".into(), "shown".into());
        assert_eq!(
            reference.resolve(&message).unwrap(),
            serde_json::json!({
                "type": "with_attachments",
                "text": "shown",
                "attachments": [image],
            })
        );
    }

    #[test]
    fn persisted_tool_output_reports_a_missing_tool_result() {
        let reference = PersistedToolCallOutput::new("absent".into(), vec![], vec![]);
        let message = ChatMessage::tool_result("other".into(), "x".into());
        assert!(reference.resolve(&message).is_err());
    }
}
