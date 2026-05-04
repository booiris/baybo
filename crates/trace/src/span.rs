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
use crate::outcome::LifecycleOutcome;

/// One atomic action recorded in the trace tree.
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
    pub outcome: LifecycleOutcome,

    /// Sub-events (sanitize hits / approvals / hook degradations).
    /// In storage these live in their own `span_events` table keyed by
    /// `(span_id, seq)`; loaded eagerly with the span for convenience.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<SpanEvent>,
}

/// Closed enum of every kind of atomic action. Each variant carries its
/// own provenance and result fields — no superset struct, no
/// `serde_json::Value` payload as a backdoor (with the explicit
/// exception of `ToolCall.params` / `ToolCall.output` because tool
/// schemas are dynamic).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpanKind {
    LlmCall {
        // Provenance (set at begin-time)
        model_id: String,
        provider: String,
        provider_config_hash: String,

        // Input (set at begin-time)
        input_messages: Vec<ChatMessage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        temperature: Option<f32>,

        // Output (filled in at end-time)
        #[serde(default, skip_serializing_if = "String::is_empty")]
        output_content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<LlmToolCallRecord>,
        #[serde(default)]
        input_tokens: usize,
        #[serde(default)]
        output_tokens: usize,
    },
    ToolCall {
        // Provenance
        tool_name: String,
        tool_artifact_hash: String,
        /// Pairing back to the LLM `Span` that emitted the tool_use
        /// block. Empty when the tool call did not originate from an
        /// LLM (e.g. `TriggerAction::ToolCall` — cron's direct invoke).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        triggered_by: Option<ToolCallOrigin>,
        // I/O — dynamic shapes per tool schema
        params: Value,
        #[serde(default)]
        output: Value,
        #[serde(default)]
        success: bool,
    },
    /// Inside a `StepKind::Subagent`. Carries no real execution state
    /// — bounds the parent's wait window. The actual work runs in
    /// `child_session_id`'s own trace tree.
    SubagentStub {
        child_session_id: aura_model::SessionId,
    },
}

impl SpanKind {
    pub fn tag(&self) -> &'static str {
        match self {
            SpanKind::LlmCall { .. } => "llm_call",
            SpanKind::ToolCall { .. } => "tool_call",
            SpanKind::SubagentStub { .. } => "subagent_stub",
        }
    }
}

/// One tool_use block emitted by an LLM. Recorded inside the LLM
/// span's `tool_calls` so the next iteration's tool spans can pair
/// back via `ToolCallOrigin`.
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

/// Opaque handle returned by `SpanRecorder::begin_span`. Carries enough
/// context to call `end_span(handle, outcome)` later. Lives in
/// `aura-trace` because it is a pure data type, not a persistence
/// behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanHandle {
    pub span_id: SpanId,
    pub step_id: StepId,
}

impl SpanHandle {
    pub fn new(span_id: SpanId, step_id: StepId) -> Self {
        Self { span_id, step_id }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_llm() -> SpanKind {
        SpanKind::LlmCall {
            model_id: "claude-sonnet-4-6".into(),
            provider: "anthropic".into(),
            provider_config_hash: "cfg-hash".into(),
            input_messages: vec![],
            temperature: Some(0.7),
            output_content: String::new(),
            thinking: None,
            tool_calls: vec![],
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    fn dummy_tool() -> SpanKind {
        SpanKind::ToolCall {
            tool_name: "bash".into(),
            tool_artifact_hash: "tool-hash".into(),
            triggered_by: Some(ToolCallOrigin {
                llm_span_id: SpanId::new(),
                tool_use_id: "tu-1".into(),
            }),
            params: serde_json::json!({"cmd": "ls"}),
            output: serde_json::json!(null),
            success: false,
        }
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
            outcome: LifecycleOutcome::Pending,
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
            outcome: LifecycleOutcome::Ok,
            events: vec![],
        };
        let s = serde_json::to_string(&span).unwrap();
        let back: Span = serde_json::from_str(&s).unwrap();
        assert_eq!(back.parallel_group, Some(pg));
        assert_eq!(back.kind, kind);
    }

    #[test]
    fn subagent_stub_round_trips() {
        let span = Span {
            id: SpanId::new(),
            step_id: StepId::new(),
            kind: SpanKind::SubagentStub {
                child_session_id: aura_model::SessionId::from("cli-child"),
            },
            parallel_group: None,
            started_at: Utc::now(),
            ended_at: None,
            outcome: LifecycleOutcome::Pending,
            events: vec![],
        };
        let s = serde_json::to_string(&span).unwrap();
        let back: Span = serde_json::from_str(&s).unwrap();
        assert_eq!(back, span);
    }

    #[test]
    fn span_handle_is_constructible() {
        let h = SpanHandle::new(SpanId::new(), StepId::new());
        assert_eq!(h.span_id, h.span_id);
    }
}
