//! `SpanEvent` — zero-duration markers attached to a `Span`.
//!
//! Sanitize hits and approval decisions surface here. Compound key is
//! `(span_id, seq)` — `seq` is span-local and starts at 0 for the first
//! event on a given span. Cross-span queries join by `event_kind` when
//! needed.

use aura_model::{ApprovalDecision, PlaceholderId, ResourceAccess, SecretKind, SpanId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Discrete observation tied to a specific `Span`. Audit-only — writing
/// a `SpanEvent` does not trigger any side effects beyond persistence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpanEvent {
    pub span_id: SpanId,
    pub seq: u32,
    pub at: DateTime<Utc>,
    pub kind: SpanEventKind,
}

/// What kind of event was recorded. Closed enum — extend by adding
/// variants, never by string tag. New audit categories should declare
/// themselves explicitly so downstream consumers can `match` exhaustively.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpanEventKind {
    /// Sanitize actually modified content. **Misses are not recorded**
    /// — the trace records what happened, not what ran.
    SanitizeHit {
        hits_count: usize,
        kinds: Vec<SecretKind>,
        /// Placeholder IDs minted for this hit, so replay can resolve
        /// them via `SecretVault`.
        placeholder_ids: Vec<PlaceholderId>,
    },
    /// **Every** approval decision is recorded — `Approve`,
    /// `ApproveAlways`, and `Deny` alike. The audit trail of "what
    /// did the user approve and when" is complete by design.
    Approval {
        decision: ApprovalDecision,
        resource: ResourceAccess,
    },
    /// Tool-emitted phase artifact. Each entry corresponds to one
    /// invocation of `ToolEventSink::emit` inside the tool body — the
    /// elapsed time of a sub-action, an HTTP response summary, a
    /// side-LLM round-trip, etc. The agent layer drains the tool's
    /// event buffer after execution, runs sanitization over text
    /// payload fields, then emits one `SpanEvent` per entry.
    ToolEvent {
        action: String,
        payload: ToolEventPayload,
    },
}

/// Payload carried inside [`SpanEventKind::ToolEvent`]. Closed enum —
/// each variant has a concrete shape so downstream consumers can
/// `match` exhaustively and the trace UI can render structured fields
/// instead of opaque JSON blobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolEventPayload {
    /// Duration of a sub-action inside a tool body. Emitted via the
    /// RAII `ToolTimer` guard returned by `start_timer`.
    Phase { duration_ms: u64 },
    /// Summary of an outbound HTTP request made by a tool. `body_preview`
    /// is the producer-truncated rendering of the response body — the
    /// raw full content is *not* persisted to the trace.
    HttpFetch {
        status: u16,
        bytes: u64,
        content_type: Option<String>,
        body_preview: Option<String>,
    },
    /// Summary of a side-LLM call made by a tool (e.g. WebFetch's
    /// summary path). `input` and `output` are producer-truncated; the
    /// executor still runs them through the leak detector before
    /// persistence.
    LlmCall {
        model: String,
        input: String,
        output: String,
    },
}

impl SpanEventKind {
    pub fn tag(&self) -> &'static str {
        match self {
            SpanEventKind::SanitizeHit { .. } => "sanitize_hit",
            SpanEventKind::Approval { .. } => "approval",
            SpanEventKind::ToolEvent { .. } => "tool_event",
        }
    }
}

impl SpanEvent {
    pub fn new(span_id: SpanId, seq: u32, kind: SpanEventKind) -> Self {
        Self {
            span_id,
            seq,
            at: Utc::now(),
            kind,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn sanitize_hit_round_trips() {
        let span = SpanId::new();
        let e = SpanEvent::new(
            span,
            0,
            SpanEventKind::SanitizeHit {
                hits_count: 2,
                kinds: vec![SecretKind::ApiKey, SecretKind::BearerToken],
                placeholder_ids: vec![PlaceholderId::new("[{REDACTED_SECRET_abc}]")],
            },
        );
        let s = serde_json::to_string(&e).unwrap();
        let back: SpanEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn tool_event_phase_round_trips() {
        let span = SpanId::new();
        let e = SpanEvent::new(
            span,
            2,
            SpanEventKind::ToolEvent {
                action: "http_request".into(),
                payload: ToolEventPayload::Phase { duration_ms: 42 },
            },
        );
        let s = serde_json::to_string(&e).unwrap();
        let back: SpanEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
        // Lock the wire tag so the web UI can `case 'tool_event':`.
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["kind"]["kind"], "tool_event");
        assert_eq!(v["kind"]["action"], "http_request");
        assert_eq!(v["kind"]["payload"]["type"], "phase");
        assert_eq!(v["kind"]["payload"]["duration_ms"], 42);
    }

    #[test]
    fn tool_event_http_fetch_round_trips() {
        let span = SpanId::new();
        let e = SpanEvent::new(
            span,
            3,
            SpanEventKind::ToolEvent {
                action: "http_request".into(),
                payload: ToolEventPayload::HttpFetch {
                    status: 200,
                    bytes: 1234,
                    content_type: Some("text/html".into()),
                    body_preview: Some("<html>…</html>".into()),
                },
            },
        );
        let s = serde_json::to_string(&e).unwrap();
        let back: SpanEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["kind"]["payload"]["type"], "http_fetch");
        assert_eq!(v["kind"]["payload"]["status"], 200);
    }

    #[test]
    fn tool_event_llm_call_round_trips() {
        let span = SpanId::new();
        let e = SpanEvent::new(
            span,
            4,
            SpanEventKind::ToolEvent {
                action: "llm_summary".into(),
                payload: ToolEventPayload::LlmCall {
                    model: "gpt-4o-mini".into(),
                    input: "summarise: foo".into(),
                    output: "foo is a value.".into(),
                },
            },
        );
        let s = serde_json::to_string(&e).unwrap();
        let back: SpanEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["kind"]["payload"]["type"], "llm_call");
        assert_eq!(v["kind"]["payload"]["model"], "gpt-4o-mini");
    }

    #[test]
    fn approval_round_trips() {
        let span = SpanId::new();
        let e = SpanEvent::new(
            span,
            1,
            SpanEventKind::Approval {
                decision: ApprovalDecision::Approve,
                resource: ResourceAccess::ReadFile {
                    path: PathBuf::from("/tmp/foo"),
                },
            },
        );
        let s = serde_json::to_string(&e).unwrap();
        let back: SpanEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
    }
}
