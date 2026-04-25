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
///
/// `bot_id` identifies the per-tenant credential that originated an
/// inbound message (for channels that multiplex many bots — Telegram,
/// future Discord). Empty string for channels or flows without a bot
/// concept (the TUI, or a single-bot sidecar). Consumed by the
/// pairing gate so the `(channel_type, bot_id, user_id)` triple can
/// gate messages per-bot. Additive; default empty keeps old sidecars
/// wire-compatible.
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
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bot_id: String,
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
    /// field is left empty — the channel auth middleware has already
    /// validated the vault-issued TUI token from the upgrade header.
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
    /// surface may drop this. `user_id` mirrors the Message frame so
    /// sidecars that route outbound by platform user (Telegram chat,
    /// Discord DM) don't need a `session_id → user` reverse map.
    /// Empty string for non-user-addressed emissions (cron, system).
    Delta {
        session_id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        user_id: String,
        text: String,
    },
    /// Server -> client: out-of-band notice surfaced by the agent
    /// (skill warnings, degraded-mode banners). `level` is a lower-
    /// case string (`"warn"` / `"error"`) so third-party clients don't
    /// need a typed enum to render it.
    Notice {
        session_id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        user_id: String,
        level: String,
        text: String,
    },
    /// Server -> client: a tool call is blocked waiting for the
    /// channel's user to approve or deny. Clients with an approval UX
    /// should echo a [`Frame::ResolveApproval`] back; clients without
    /// one can ignore, and the gate will time out server-side.
    /// `user_id` identifies the platform user so sidecars can post the
    /// prompt into the right chat without maintaining a `session_id`
    /// map.
    ApprovalRequested {
        call_id: String,
        session_id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        user_id: String,
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
    /// Client -> server: a log line emitted by the sidecar itself,
    /// forwarded so aura operators see sidecar output in the dashboard
    /// alongside gateway-internal tracing. The server attributes the
    /// record to the sidecar's `ChannelType` before handing it to the
    /// `LogBuffer`.
    ///
    /// `level` mirrors lower-cased tracing levels (`"error"`, `"warn"`,
    /// `"info"`, `"debug"`). Unknown values degrade to `info`. `target`
    /// is an optional module/category the sidecar tags. Additive frame —
    /// no `PROTOCOL_VERSION` bump.
    SidecarLog {
        level: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts-export", ts(optional))]
        target: Option<String>,
    },
    /// Server -> client: attach a new bot (or other per-tenant
    /// credential) to a sidecar that multiplexes many tenants over
    /// one WS. For the Telegram channel `bot_id` is an operator-chosen
    /// stable label and `token` is the @BotFather token. Sidecars
    /// reply with [`Frame::BotStatus`] to ack; a failure on
    /// startup is surfaced via `ok: false + message`.
    StartBot { bot_id: String, token: String },
    /// Server -> client: detach a previously-attached bot. The
    /// sidecar stops polling for that bot and drops its in-process
    /// state. Any in-flight approval / message tied to that bot is
    /// abandoned (aura's own side cleans up state independently).
    StopBot { bot_id: String },
    /// Client -> server: ack for a `StartBot`/`StopBot` command.
    /// `ok: true` means the command ran; `ok: false` carries a
    /// human-readable reason so aura can surface it (e.g. bad token).
    BotStatus {
        bot_id: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts-export", ts(optional))]
        message: Option<String>,
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
            bot_id: "prod-bot".into(),
        });
        let bytes = encode(&frame).unwrap();
        assert_eq!(frame, decode(&bytes).unwrap());
    }

    #[test]
    fn round_trip_message_without_bot_id_decodes_empty() {
        // Old sidecars that predate `bot_id` encode four fields.
        // The additive schema must still decode — serde_default fills
        // in the empty string.
        let frame = Frame::Message(Message {
            content: "hi".into(),
            session_id: "s1".into(),
            user_id: "u1".into(),
            channel_type: ChannelType::from("slack"),
            bot_id: String::new(),
        });
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
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
            user_id: "u1".into(),
            text: "hel".into(),
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_notice() {
        let frame = Frame::Notice {
            session_id: "s1".into(),
            user_id: "u1".into(),
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
            user_id: "u1".into(),
            tool: "fs.read".into(),
            accesses: vec![ResourceAccess::ReadFile {
                path: PathBuf::from("/tmp/x"),
            }],
            params_preview: "{}".into(),
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_delta_legacy_without_user_id_decodes() {
        // Old server that predates the `user_id` field encodes only
        // session_id + text. The additive schema must still decode —
        // serde_default fills in the empty user_id.
        let frame = Frame::Delta {
            session_id: "s1".into(),
            user_id: String::new(),
            text: "chunk".into(),
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

    #[test]
    fn round_trip_sidecar_log_with_target() {
        let frame = Frame::SidecarLog {
            level: "warn".into(),
            text: "retry exhausted".into(),
            target: Some("telegram::poll".into()),
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_start_bot() {
        let frame = Frame::StartBot {
            bot_id: "prod-bot".into(),
            token: "123:ABC".into(),
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_stop_bot() {
        let frame = Frame::StopBot {
            bot_id: "prod-bot".into(),
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_bot_status_ok_without_message() {
        let frame = Frame::BotStatus {
            bot_id: "prod-bot".into(),
            ok: true,
            message: None,
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_bot_status_error_with_message() {
        let frame = Frame::BotStatus {
            bot_id: "prod-bot".into(),
            ok: false,
            message: Some("401 Unauthorized".into()),
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_sidecar_log_without_target() {
        let frame = Frame::SidecarLog {
            level: "info".into(),
            text: "startup".into(),
            target: None,
        };
        let bytes = encode(&frame).unwrap();
        // `target: None` is omitted on the wire — old peers that don't
        // send the field must still decode into the None variant.
        assert_eq!(frame, decode(&bytes).unwrap());
    }
}
