//! On-the-wire event shapes shared between the gateway SSE endpoints
//! and plugin-side clients (TUI today, third-party channel plugins
//! tomorrow). Kept in `aura-channels` so server and client compile
//! against a single canonical definition — previously these types were
//! duplicated in `aura-gateway::http_adapter` and
//! `aura-tui::client::dto`, which required the two copies to stay
//! byte-for-byte identical across every schema change.

use aura_tools::{ApprovalDecision, ResourceAccess};
use serde::{Deserialize, Serialize};

/// SSE event payload pushed to clients subscribed to a session stream
/// at `GET /v1/sessions/:id/stream`.
///
/// Serialised by the gateway, deserialised by channel-plugin clients.
/// The `kind` tag + `snake_case` rename are the wire contract — changes
/// to the variant set or fields are breaking.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SseEvent {
    /// Incremental assistant text chunk.
    Delta { text: String },
    /// Final assistant response for the turn (content as rendered text).
    Response { text: String },
    /// Out-of-band notice surfaced by the agent.
    Notice { level: String, text: String },
}

/// Event pushed on the gateway-wide approval stream
/// (`GET /v1/approvals/stream`).
///
/// Separate from [`SseEvent`] because approvals live on their own SSE
/// endpoint, keyed by channel type rather than chat session. Same wire
/// contract rules apply.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalEvent {
    /// A new approval was queued. Clients should re-fetch
    /// `GET /v1/approvals` to see the full list.
    Added {
        call_id: String,
        session_id: String,
        tool: String,
        accesses: Vec<ResourceAccess>,
        params_preview: String,
    },
    /// An approval was resolved by any client; remove `call_id` from
    /// the local UI (decisions other than the current client's also
    /// land here so multiple frontends stay in sync).
    Resolved {
        call_id: String,
        decision: ApprovalDecision,
    },
}

#[cfg(test)]
mod tests {
    //! Round-trip tests that pin the JSON wire format. With the types
    //! unified, these are the contract between gateway serialiser and
    //! plugin deserialiser — a shape regression fails here first.

    use super::*;
    use serde_json::{Value, json};

    fn round_trip_sse(event: SseEvent, expected: Value) {
        let encoded = serde_json::to_value(&event).unwrap();
        assert_eq!(encoded, expected);
        let decoded: SseEvent = serde_json::from_value(expected).unwrap();
        // Match on decoded to confirm variant + fields round-trip.
        match (event, decoded) {
            (SseEvent::Delta { text: a }, SseEvent::Delta { text: b }) => assert_eq!(a, b),
            (SseEvent::Response { text: a }, SseEvent::Response { text: b }) => assert_eq!(a, b),
            (
                SseEvent::Notice {
                    level: la,
                    text: ta,
                },
                SseEvent::Notice {
                    level: lb,
                    text: tb,
                },
            ) => {
                assert_eq!(la, lb);
                assert_eq!(ta, tb);
            }
            (a, b) => panic!("variant mismatch: {a:?} vs {b:?}"),
        }
    }

    #[test]
    fn sse_delta_wire_shape() {
        round_trip_sse(
            SseEvent::Delta {
                text: "hello".into(),
            },
            json!({ "kind": "delta", "text": "hello" }),
        );
    }

    #[test]
    fn sse_response_wire_shape() {
        round_trip_sse(
            SseEvent::Response {
                text: "done".into(),
            },
            json!({ "kind": "response", "text": "done" }),
        );
    }

    #[test]
    fn sse_notice_wire_shape() {
        round_trip_sse(
            SseEvent::Notice {
                level: "warn".into(),
                text: "heads up".into(),
            },
            json!({ "kind": "notice", "level": "warn", "text": "heads up" }),
        );
    }

    #[test]
    fn approval_added_wire_shape() {
        let event = ApprovalEvent::Added {
            call_id: "c1".into(),
            session_id: "s1".into(),
            tool: "fs.read".into(),
            accesses: vec![],
            params_preview: "{}".into(),
        };
        let encoded = serde_json::to_value(&event).unwrap();
        assert_eq!(
            encoded,
            json!({
                "kind": "added",
                "call_id": "c1",
                "session_id": "s1",
                "tool": "fs.read",
                "accesses": [],
                "params_preview": "{}",
            })
        );
        let decoded: ApprovalEvent = serde_json::from_value(encoded).unwrap();
        match decoded {
            ApprovalEvent::Added {
                call_id,
                session_id,
                tool,
                accesses,
                params_preview,
            } => {
                assert_eq!(call_id, "c1");
                assert_eq!(session_id, "s1");
                assert_eq!(tool, "fs.read");
                assert!(accesses.is_empty());
                assert_eq!(params_preview, "{}");
            }
            other => panic!("expected Added, got {other:?}"),
        }
    }

    #[test]
    fn approval_resolved_wire_shape() {
        let event = ApprovalEvent::Resolved {
            call_id: "c1".into(),
            decision: ApprovalDecision::Approve,
        };
        let encoded = serde_json::to_value(&event).unwrap();
        assert_eq!(
            encoded,
            json!({
                "kind": "resolved",
                "call_id": "c1",
                "decision": "approve",
            })
        );
        let decoded: ApprovalEvent = serde_json::from_value(encoded).unwrap();
        match decoded {
            ApprovalEvent::Resolved { call_id, decision } => {
                assert_eq!(call_id, "c1");
                assert!(matches!(decision, ApprovalDecision::Approve));
            }
            other => panic!("expected Resolved, got {other:?}"),
        }
    }
}
