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
///
/// **Lifecycle invariant:** `outcome.is_terminal() ⟺ ended_at.is_some()`.
/// Mutate the two together via [`Span::close`]; never set one without
/// the other. Recovery, storage rewrites, and replay all rely on this
/// pairing.
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

    /// Sub-events (sanitize hits / approvals).
    /// In storage these live in their own `span_events` table keyed by
    /// `(span_id, seq)`; loaded eagerly with the span for convenience.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<SpanEvent>,
}

impl Span {
    /// Atomically transition this span to a terminal outcome. Sets
    /// both `outcome` and `ended_at` from the single call so the
    /// lifecycle invariant cannot be violated. Returns an error if
    /// `outcome` is `LifecycleOutcome::Pending`.
    pub fn close(&mut self, outcome: LifecycleOutcome, at: DateTime<Utc>) -> crate::Result<()> {
        if !outcome.is_terminal() {
            return Err(crate::TraceError::InvalidOperation(format!(
                "Span::close requires a terminal LifecycleOutcome, got {}",
                outcome.tag()
            )));
        }
        self.outcome = outcome;
        self.ended_at = Some(at);
        Ok(())
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

/// Begin-time data for an `LlmCall` span — set when the request is
/// dispatched, never mutated afterwards.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmCallBegin {
    pub model_id: String,
    pub provider: String,
    pub provider_config_hash: String,
    pub input_messages: Vec<ChatMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

/// End-time result for an `LlmCall` span — set when the response is
/// received. `None` while the span is `Pending` or if it cancelled
/// before producing a result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
}

/// Begin-time data for a `ToolCall` span.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallBegin {
    pub tool_name: String,
    pub tool_artifact_hash: String,
    /// Pairing back to the LLM `Span` that emitted the tool_use block.
    /// `None` when the tool call did not originate from an LLM (e.g.
    /// `TriggerAction::ToolCall` — cron's direct invoke).
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
                input_messages: vec![],
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
            outcome: LifecycleOutcome::Pending,
            events: vec![],
        }
    }

    #[test]
    fn close_pairs_outcome_and_ended_at() {
        let mut span = pending_span(dummy_llm());
        let now = Utc::now();
        span.close(LifecycleOutcome::Ok, now).unwrap();
        assert_eq!(span.outcome, LifecycleOutcome::Ok);
        assert_eq!(span.ended_at, Some(now));
    }

    #[test]
    fn close_rejects_pending() {
        let mut span = pending_span(dummy_tool());
        let err = span
            .close(LifecycleOutcome::Pending, Utc::now())
            .unwrap_err();
        assert!(matches!(err, crate::TraceError::InvalidOperation(_)));
        assert!(span.ended_at.is_none());
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
        let h = SpanHandle::new(SpanId::new(), StepId::new(), dummy_llm(), Utc::now(), None);
        assert_eq!(h.span_id, h.clone().span_id);
    }
}
