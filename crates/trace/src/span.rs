//! `Span` — one atomic action (LLM call or tool call) with a start /
//! end window and a closed strong-typed `kind`.
//!
//! Spans are direct children of `Step`s. Spans within a step may run in
//! parallel (sibling spans sharing a `parallel_group: ParallelGroup`)
//! but never nest. LLM ↔ tool pairing is by `ToolCallOrigin`, not by
//! tree structure.

use aura_model::{ChatMessage, ParallelGroup, SpanId, StepId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
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
/// **Storage coupling:** the `spans` table in `aura-storage` derives its
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
    pub provider_config_hash: String,
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
    #[serde(default)]
    pub input_tokens: usize,
    #[serde(default)]
    pub output_tokens: usize,
    /// Anthropic prompt-cache: input tokens served from the cache.
    /// `#[serde(default)]` keeps already-persisted spans (which lack
    /// the field) decodable.
    #[serde(default)]
    pub cached_input_tokens: usize,
    /// Anthropic prompt-cache: input tokens written into the cache.
    #[serde(default)]
    pub cache_creation_input_tokens: usize,
}

/// Begin-time data for a `ToolCall` span.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallBegin {
    pub tool_name: String,
    pub tool_artifact_hash: String,
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
    #[serde(default)]
    pub output: Value,
    #[serde(default)]
    pub success: bool,
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
                provider_config_hash: "cfg-hash".into(),
                input_messages: LlmCallInputs::empty(),
                temperature: Some(0.7),
            },
            result: None,
        }
    }

    fn dummy_tool() -> SpanKind {
        SpanKind::ToolCall {
            begin: ToolCallBegin {
                tool_name: "bash".into(),
                tool_artifact_hash: "tool-hash".into(),
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
        let inline = LlmCallInputs::Inline(vec![aura_model::ChatMessage {
            role: aura_model::Role::User,
            content: vec![aura_model::ContentBlock::Text("hi".into())],
            from_user: false,
        }]);
        let json = serde_json::to_value(&inline).unwrap();
        assert!(json.is_array(), "Inline must serialize as a bare array");
        assert_eq!(json.as_array().unwrap().len(), 1);

        let persisted = LlmCallInputs::Persisted { last_ordinal: 7 };
        let json = serde_json::to_value(&persisted).unwrap();
        assert!(json.is_object(), "Persisted must serialize as an object");
        assert_eq!(json["last_ordinal"], 7);

        // Round-trip both shapes back through Deserialize.
        let v1: LlmCallInputs = serde_json::from_value(serde_json::json!([])).unwrap();
        assert!(matches!(v1, LlmCallInputs::Inline(_)));
        let v2: LlmCallInputs =
            serde_json::from_value(serde_json::json!({"last_ordinal": 1})).unwrap();
        assert!(matches!(v2, LlmCallInputs::Persisted { .. }));
    }
}
