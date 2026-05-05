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
}

impl SpanEventKind {
    pub fn tag(&self) -> &'static str {
        match self {
            SpanEventKind::SanitizeHit { .. } => "sanitize_hit",
            SpanEventKind::Approval { .. } => "approval",
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
