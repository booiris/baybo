//! Wire protocol shared by the gateway channel server and every
//! channel sidecar client.
//!
//! The only out-of-tree consumer is the TypeScript package under
//! `sdks/channel-ts/`, which reuses the ts-rs-generated bindings below
//! and the MessagePack encoding to speak the same protocol. The
//! built-in TUI has a private Rust WS client (`crates/tui/src/client/
//! ws.rs`) that rides on these same types.

use aura_model::{ChannelType, ResourceAccess};
use aura_tools::ApprovalDecision;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Wire-format version this protocol speaks. Bump on breaking frame changes.
pub const PROTOCOL_VERSION: u16 = 1;

/// Error surface for frame encode/decode.
#[derive(Debug, Error)]
pub enum WireError {
    #[error("encode: {0}")]
    Encode(#[from] rmp_serde::encode::Error),

    #[error("decode: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
}

/// The canonical sidecar-to-aura (or aura-to-sidecar) message. A single
/// connection may carry messages for many `user_id`s — the sidecar
/// multiplexes its users onto one WebSocket.
///
/// `channel_type` holds an [`aura_model::ChannelType`] but exports to
/// TypeScript as a plain `string` — the domain type is a transparent
/// newtype over `String`, and we don't want ts-rs to pull the domain
/// crate into its generated schema.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sdks/channel-ts/src/generated/")
)]
pub struct Message {
    pub content: String,
    pub session_id: String,
    pub user_id: String,
    #[cfg_attr(feature = "ts-export", ts(type = "string"))]
    pub channel_type: ChannelType,
}

/// Frame envelope. Tagged on the `kind` field so the receive side
/// never has to guess. Encoded with
/// [`rmp_serde::to_vec_named`](rmp_serde::to_vec_named) so field names
/// round-trip — makes it trivial to hand-write a TypeScript decoder.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sdks/channel-ts/src/generated/")
)]
pub enum Frame {
    /// First frame after the WebSocket handshake. Carries the
    /// sidecar's capability token (injected via `AURA_CHANNEL_TOKEN`)
    /// and its declared channel type. For the built-in TUI the token
    /// field is left empty — PSK auth already happened on the WS
    /// upgrade request.
    ///
    /// `session_id` distinguishes two flavors of client:
    /// * `None` — **sidecar**. One process serves every user of this
    ///   channel type; the registry enforces a 1:1 `ChannelType →
    ///   Channel` mapping.
    /// * `Some(sid)` — **session-scoped client** (the built-in TUI
    ///   today). Multiple such clients of the same channel type may
    ///   coexist as long as their session ids differ. Agent output for
    ///   `sid` is routed back to this specific connection.
    Register {
        token: String,
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        channel_type: ChannelType,
        protocol_version: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts-export", ts(optional))]
        session_id: Option<String>,
    },
    /// Server response to `Register`. `ok: false` carries a
    /// human-readable reason.
    RegisterAck { ok: bool, reason: Option<String> },
    /// A user-visible message flowing in either direction. Inbound
    /// from a channel it is user input; outbound it is the agent's
    /// final response for a turn.
    Message(Message),
    /// Server -> client: incremental assistant text chunk for the
    /// in-flight response on a session. Channels without a partial
    /// surface may drop this.
    Delta { session_id: String, text: String },
    /// Server -> client: out-of-band notice surfaced by the agent
    /// (skill warnings, degraded-mode banners). `level` is a lower-
    /// case string (`"warn"` / `"error"`) so third-party clients don't
    /// need a typed enum to render it.
    Notice {
        session_id: String,
        level: String,
        text: String,
    },
    /// Server -> client: a tool call is blocked waiting for the
    /// channel's user to approve or deny. Clients with an approval UX
    /// should echo a [`Frame::ResolveApproval`] back; clients without
    /// one can ignore, and the gate will time out server-side.
    ApprovalRequested {
        call_id: String,
        session_id: String,
        tool: String,
        #[cfg_attr(feature = "ts-export", ts(type = "unknown[]"))]
        accesses: Vec<ResourceAccess>,
        params_preview: String,
    },
    /// Server -> client: any client (including ours) resolved the
    /// approval — drop it from the local UI so concurrent frontends
    /// stay consistent.
    ApprovalResolved {
        call_id: String,
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        decision: ApprovalDecision,
    },
    /// Client -> server: resolve a pending approval the client
    /// previously saw in an [`Frame::ApprovalRequested`].
    ResolveApproval {
        call_id: String,
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        decision: ApprovalDecision,
    },
    /// Client -> server: persist one submitted input line to the
    /// server-side history store. Used by the built-in TUI to get
    /// zsh-style history without the client holding any encryption key
    /// itself — the gateway's [`aura_security::SecretVault`] is the
    /// single writer. Fire-and-forget; the server does not ack
    /// per-append.
    HistoryAppend { session_id: String, entry: String },
    /// Server -> client: the full history ring the TUI should rehydrate
    /// its in-memory scrollback from. Sent exactly once right after
    /// `RegisterAck { ok: true }` for session-scoped TUI clients.
    /// Sidecars never receive this.
    HistorySnapshot {
        session_id: String,
        entries: Vec<String>,
    },
}

/// Serialize a frame with named fields (MessagePack map representation).
pub fn encode(frame: &Frame) -> Result<Vec<u8>, WireError> {
    rmp_serde::to_vec_named(frame).map_err(WireError::from)
}

/// Deserialize a frame.
pub fn decode(bytes: &[u8]) -> Result<Frame, WireError> {
    rmp_serde::from_slice(bytes).map_err(WireError::from)
}

/// Regenerates `sdks/channel-ts/src/generated/constants.ts` with
/// Rust-authored values so the TS SDK doesn't keep its own copy of
/// `PROTOCOL_VERSION` (and any future wire-level constants).
/// Pairs with the `ts-rs`-driven type exports above; both run under
/// the same `ts-export` feature gate and land in the same directory.
#[cfg(all(test, feature = "ts-export"))]
#[test]
fn export_constants() {
    let out = format!(
        "// This file was generated by aura-channels. Do not edit this file manually.\n\
         export const PROTOCOL_VERSION = {PROTOCOL_VERSION};\n"
    );
    std::fs::write("../../sdks/channel-ts/src/generated/constants.ts", out)
        .expect("write generated constants.ts");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_register() {
        let frame = Frame::Register {
            token: "deadbeef".into(),
            channel_type: ChannelType::from("slack"),
            protocol_version: PROTOCOL_VERSION,
            session_id: None,
        };
        let bytes = encode(&frame).unwrap();
        let back = decode(&bytes).unwrap();
        assert_eq!(frame, back);
    }

    #[test]
    fn round_trip_register_session_scoped() {
        let frame = Frame::Register {
            token: String::new(),
            channel_type: ChannelType::from("tui"),
            protocol_version: PROTOCOL_VERSION,
            session_id: Some("sess-abc".into()),
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn register_without_session_field_decodes_with_none() {
        // Old sidecars that predate `session_id` encode only three
        // fields. The additive schema must still deserialize — treating
        // the missing field as None keeps the wire protocol at v1 for
        // backward compatibility.
        let frame = Frame::Register {
            token: "deadbeef".into(),
            channel_type: ChannelType::from("slack"),
            protocol_version: PROTOCOL_VERSION,
            session_id: None,
        };
        let encoded = encode(&frame).unwrap();
        // The encoded form should not contain a session_id key because
        // `skip_serializing_if` omits it when None.
        let decoded = decode(&encoded).unwrap();
        assert_eq!(frame, decoded);
    }

    #[test]
    fn round_trip_message() {
        let frame = Frame::Message(Message {
            content: "hi".into(),
            session_id: "s1".into(),
            user_id: "u1".into(),
            channel_type: ChannelType::from("slack"),
        });
        let bytes = encode(&frame).unwrap();
        assert_eq!(frame, decode(&bytes).unwrap());
    }

    #[test]
    fn round_trip_ack_rejected() {
        let frame = Frame::RegisterAck {
            ok: false,
            reason: Some("bad token".into()),
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_delta() {
        let frame = Frame::Delta {
            session_id: "s1".into(),
            text: "hel".into(),
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_notice() {
        let frame = Frame::Notice {
            session_id: "s1".into(),
            level: "warn".into(),
            text: "heads up".into(),
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_approval_requested() {
        use std::path::PathBuf;
        let frame = Frame::ApprovalRequested {
            call_id: "c1".into(),
            session_id: "s1".into(),
            tool: "fs.read".into(),
            accesses: vec![ResourceAccess::ReadFile {
                path: PathBuf::from("/tmp/x"),
            }],
            params_preview: "{}".into(),
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_approval_resolved() {
        let frame = Frame::ApprovalResolved {
            call_id: "c1".into(),
            decision: ApprovalDecision::Approve,
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_resolve_approval() {
        let frame = Frame::ResolveApproval {
            call_id: "c1".into(),
            decision: ApprovalDecision::Deny,
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_history_append() {
        let frame = Frame::HistoryAppend {
            session_id: "sess-1".into(),
            entry: "echo hello".into(),
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_history_snapshot() {
        let frame = Frame::HistorySnapshot {
            session_id: "sess-1".into(),
            entries: vec!["one".into(), "two".into(), "three".into()],
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }
}
