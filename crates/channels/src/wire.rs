//! Wire protocol shared by the gateway channel server and every
//! channel sidecar client.
//!
//! The only out-of-tree consumer is the TypeScript package under
//! `sdks/channel-ts/`, which reuses the ts-rs-generated bindings below
//! and the MessagePack encoding to speak the same protocol. The
//! built-in TUI has a private Rust WS client (`crates/tui/src/client/
//! ws.rs`) that rides on these same types.

use std::collections::HashMap;

use aura_model::{ChannelType, ResourceAccess};
use aura_tools::ApprovalDecision;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Error surface for frame encode/decode.
#[derive(Debug, Error)]
pub enum WireError {
    #[error("encode: {0}")]
    Encode(#[from] rmp_serde::encode::Error),

    #[error("decode: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
}

/// Discriminator on a [`WireAttachment`]. Maps 1:1 to the matching
/// [`aura_model::ContentBlock`] variant on either side of the bridge.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sdks/channel-ts/src/generated/")
)]
pub enum AttachmentKind {
    Image,
    Audio,
    File,
}

/// Reference to a media payload that travels alongside a [`Message`].
/// The bytes themselves never ride the WS — they live in the gateway's
/// `BlobStore` and are uploaded / fetched out-of-band via
/// `POST/GET /v1/blobs/*` (HTTP, same channel-token auth as the WS).
/// The wire only carries the content-addressed `blob_id` so the frame
/// stays small even for 100 MiB attachments and head-of-line blocking
/// can't take out unrelated traffic on the same connection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sdks/channel-ts/src/generated/")
)]
pub struct WireAttachment {
    pub kind: AttachmentKind,
    /// Content-addressed id from the `BlobStore` (`"sha256:<64hex>"`).
    pub blob_id: String,
    pub mime_type: String,
    /// Byte length of the underlying blob. Sidecars consume this to
    /// short-circuit a download when the platform's send limit is
    /// smaller than the blob. `u32` (4 GiB) is plenty given the
    /// gateway's 100 MiB upload cap, and avoids the TS-side
    /// `BigInt` round-trip that the default `@msgpack/msgpack`
    /// encoder rejects.
    pub size: u32,
    /// Original filename for `File` kind. Servers ignore for image /
    /// audio (where the platform usually picks its own name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub filename: Option<String>,
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
///
/// `attachments` carries non-text media (image / audio / file). The
/// bytes ride the HTTP `/v1/blobs/*` side-channel; this list only
/// references them by `blob_id`. Additive; default empty keeps old
/// sidecars wire-compatible.
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<WireAttachment>,
    /// Platform-native message id (Telegram `update_id`, Weixin
    /// `msg_id`, …). When non-empty the gateway dedups inbound traffic
    /// per `(channel_type, bot_id, platform_msg_id)` so a sidecar that
    /// replays its long-poll buffer after a restart doesn't double-fire
    /// the agent. Sidecars without a stable platform id (or that don't
    /// care to dedup) leave it empty. Additive; default empty keeps old
    /// sidecars wire-compatible.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub platform_msg_id: String,
}

/// One slash command published to a sidecar's native command surface
/// (Telegram `setMyCommands`, Discord application commands, …). The
/// gateway is the single source of truth for the command list and
/// pushes it via [`Frame::SlashManifest`] after RegisterAck so the
/// sidecar doesn't have to keep its own copy in sync.
///
/// `command` is the bare command name (no leading `/`). Telegram's
/// rule (`[a-z0-9_]{1,32}`) is the most restrictive — keep entries
/// inside it for portability across platforms.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sdks/channel-ts/src/generated/")
)]
pub struct SlashCommandSpec {
    pub command: String,
    pub description: String,
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
    ///
    /// `capabilities` advertises the optional non-core wire frames the
    /// peer can speak (e.g. `"secrets"` for [`Frame::SecretRequest`]).
    /// Empty / missing means "core frames only" — the gateway will
    /// never push a non-core frame to a sidecar that didn't claim its
    /// capability. Forward-compatible: legacy v1 sidecars that send no
    /// `capabilities` field decode here as `vec![]`. The legacy
    /// `protocol_version` field on the wire is silently ignored.
    Register {
        token: String,
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        channel_type: ChannelType,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts-export", ts(optional))]
        session_id: Option<String>,
    },
    /// Server response to `Register`. `ok: false` carries a
    /// human-readable reason. `capabilities` mirrors the gateway's own
    /// advertised set so the sidecar SDK can gate optional helpers
    /// (e.g. throw `CapabilityMissingError` from `secrets()`).
    /// Forward-compatible: legacy gateways that send no `capabilities`
    /// decode here as `vec![]`.
    RegisterAck {
        ok: bool,
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<String>,
    },
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
        /// Optional human-readable label the tool produced via
        /// `Tool::call_label` (e.g. Bash's `description` parameter).
        /// Sidecars predating this field still decode: the field
        /// deserializes to `None` and is omitted on serialize.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
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
    ///
    /// `metadata` carries channel-specific auxiliary credentials and
    /// configuration the sidecar needs at startup beyond the primary
    /// `token` — e.g. Lark's `(app_secret, encrypt_key, verification_token,
    /// base_url)` quartet, Discord intents bitmask, Slack signing-secret.
    /// Free-form key/value strings; the gateway treats them opaquely
    /// and just plumbs the row's stored map through. Default empty for
    /// single-secret channels (Telegram, Weixin); legacy sidecars that
    /// predate this field decode it as `{}`.
    StartBot {
        bot_id: String,
        token: String,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        #[cfg_attr(feature = "ts-export", ts(type = "Record<string, string>"))]
        metadata: HashMap<String, String>,
    },
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
    /// Server -> client: gateway-authored list of slash commands the
    /// sidecar should publish on its native command surface (e.g.
    /// Telegram's `setMyCommands`). Sent once right after a successful
    /// `RegisterAck` for sidecar-flavoured connections (not session-
    /// scoped TUIs); the manifest replaces any prior list. Sidecars
    /// that don't surface client-side autocomplete may ignore this.
    /// Additive; older sidecars decode the unknown tag as a no-op.
    SlashManifest { commands: Vec<SlashCommandSpec> },
    /// Client -> server: sidecar-initiated request against the
    /// gateway's encrypted secret vault. Persists per-bot tokens
    /// (Lark per-user UATs, Discord OAuth refresh tokens, …) under a
    /// scope the gateway derives server-side as
    /// `channel.<channel_type>.bot.<bot_id>.user.<key>`. The sidecar
    /// can never claim a `bot_id` it doesn't own; the gateway
    /// validates against [`aura_storage::ChannelBotStore`]. Gated by
    /// the `"secrets"` capability advertised on
    /// [`Frame::Register`] / [`Frame::RegisterAck`].
    SecretRequest {
        request_id: String,
        bot_id: String,
        op: SecretOp,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts-export", ts(optional))]
        key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts-export", ts(optional))]
        value: Option<String>,
    },
    /// Server -> client: reply to a [`Frame::SecretRequest`]. `ok: true`
    /// fills in `value` for `Get` and `keys` for `List`. `ok: false`
    /// carries an `error` tag the SDK throws as a typed error
    /// (`bot_unknown` / `key_too_long` / `value_too_large` /
    /// `quota_exceeded` / `internal`).
    SecretReply {
        request_id: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts-export", ts(optional))]
        value: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts-export", ts(optional))]
        keys: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts-export", ts(optional))]
        error: Option<String>,
    },
}

/// Operation discriminator on a [`Frame::SecretRequest`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sdks/channel-ts/src/generated/")
)]
pub enum SecretOp {
    Get,
    Set,
    Delete,
    List,
}

/// Per-`(channel_type, bot_id)` body limits enforced by the gateway
/// before the request reaches the vault. The SDK mirrors the same
/// limits client-side so a misbehaving caller surfaces them as a
/// `RangeError` instead of an opaque server reject.
pub mod secret_limits {
    /// Hard cap on the UTF-8 byte length of a [`super::SecretOp::Set`]
    /// `value`.
    pub const MAX_VALUE_BYTES: usize = 64 * 1024;
    /// Hard cap on the UTF-8 byte length of a `key`.
    pub const MAX_KEY_BYTES: usize = 256;
    /// Maximum number of live keys per `(channel_type, bot_id)` scope.
    /// Exceeding it on `Set` is rejected with `quota_exceeded`.
    pub const MAX_KEYS_PER_SCOPE: usize = 10_000;
}

/// Serialize a frame with named fields (MessagePack map representation).
pub fn encode(frame: &Frame) -> Result<Vec<u8>, WireError> {
    rmp_serde::to_vec_named(frame).map_err(WireError::from)
}

/// Deserialize a frame.
pub fn decode(bytes: &[u8]) -> Result<Frame, WireError> {
    rmp_serde::from_slice(bytes).map_err(WireError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_register() {
        let frame = Frame::Register {
            token: "deadbeef".into(),
            channel_type: ChannelType::from("slack"),
            capabilities: vec!["secrets".into()],
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
            capabilities: Vec::new(),
            session_id: Some("sess-abc".into()),
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn register_without_session_field_decodes_with_none() {
        // Old sidecars that predate `session_id` encode only their
        // required fields. Missing optional fields must decode as
        // None / default values so the wire protocol stays
        // forward-compatible.
        let frame = Frame::Register {
            token: "deadbeef".into(),
            channel_type: ChannelType::from("slack"),
            capabilities: Vec::new(),
            session_id: None,
        };
        let encoded = encode(&frame).unwrap();
        let decoded = decode(&encoded).unwrap();
        assert_eq!(frame, decoded);
    }

    #[test]
    fn legacy_register_with_protocol_version_decodes() {
        // Old v1 sidecars send a `protocol_version: 1` field. The new
        // gateway must silently drop the unknown field (rmp-serde's
        // default for struct deserialization) and decode the rest.
        // Hand-encode the legacy shape with rmp_serde so we never
        // accidentally regress this compatibility window.
        #[derive(Serialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum LegacyFrame {
            Register {
                token: String,
                channel_type: ChannelType,
                protocol_version: u16,
                #[serde(skip_serializing_if = "Option::is_none")]
                session_id: Option<String>,
            },
        }
        let legacy = LegacyFrame::Register {
            token: "deadbeef".into(),
            channel_type: ChannelType::from("slack"),
            protocol_version: 1,
            session_id: None,
        };
        let bytes = rmp_serde::to_vec_named(&legacy).unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(
            decoded,
            Frame::Register {
                token: "deadbeef".into(),
                channel_type: ChannelType::from("slack"),
                capabilities: Vec::new(),
                session_id: None,
            }
        );
    }

    #[test]
    fn round_trip_slash_manifest() {
        let frame = Frame::SlashManifest {
            commands: vec![
                SlashCommandSpec {
                    command: "new".into(),
                    description: "Start a fresh session".into(),
                },
                SlashCommandSpec {
                    command: "help".into(),
                    description: "Show help".into(),
                },
            ],
        };
        let bytes = encode(&frame).unwrap();
        assert_eq!(frame, decode(&bytes).unwrap());
    }

    #[test]
    fn round_trip_slash_manifest_empty() {
        let frame = Frame::SlashManifest {
            commands: Vec::new(),
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_message() {
        let frame = Frame::Message(Message {
            content: "hi".into(),
            session_id: "s1".into(),
            user_id: "u1".into(),
            channel_type: ChannelType::from("slack"),
            bot_id: "prod-bot".into(),
            attachments: Vec::new(),
            platform_msg_id: String::new(),
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
            attachments: Vec::new(),
            platform_msg_id: String::new(),
        });
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_message_with_attachments() {
        let frame = Frame::Message(Message {
            content: "look at this".into(),
            session_id: "s1".into(),
            user_id: "u1".into(),
            channel_type: ChannelType::from("weixin"),
            bot_id: "prod-bot".into(),
            attachments: vec![
                WireAttachment {
                    kind: AttachmentKind::Image,
                    blob_id: format!("sha256:{}", "0".repeat(64)),
                    mime_type: "image/png".into(),
                    size: 1024,
                    filename: None,
                },
                WireAttachment {
                    kind: AttachmentKind::File,
                    blob_id: format!("sha256:{}", "1".repeat(64)),
                    mime_type: "application/pdf".into(),
                    size: 4096,
                    filename: Some("report.pdf".into()),
                },
            ],
            platform_msg_id: String::new(),
        });
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn message_attachments_field_omitted_when_empty() {
        // Old sidecars that predate `attachments` encode the same five
        // fields they always did. The new field's `skip_serializing_if`
        // keeps the wire identical when unused, and `serde_default`
        // fills `Vec::new()` when decoding either direction.
        let frame = Frame::Message(Message {
            content: "hi".into(),
            session_id: "s1".into(),
            user_id: "u1".into(),
            channel_type: ChannelType::from("slack"),
            bot_id: String::new(),
            attachments: Vec::new(),
            platform_msg_id: String::new(),
        });
        let encoded = encode(&frame).unwrap();
        // MessagePack with `to_vec_named` writes field names as bare
        // utf-8 strings — if the `attachments` key was emitted at all,
        // the literal would be present in the output. Use that as a
        // wire-level proxy for "omitted on empty" without pulling in a
        // dynamic-value MessagePack dep.
        let as_str = String::from_utf8_lossy(&encoded);
        assert!(
            !as_str.contains("attachments"),
            "attachments key should be omitted when empty",
        );
        assert_eq!(frame, decode(&encoded).unwrap());
    }

    #[test]
    fn round_trip_ack_rejected() {
        let frame = Frame::RegisterAck {
            ok: false,
            reason: Some("bad token".into()),
            capabilities: Vec::new(),
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_ack_with_capabilities() {
        let frame = Frame::RegisterAck {
            ok: true,
            reason: None,
            capabilities: vec!["secrets".into(), "abort".into()],
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
            description: Some("read x".into()),
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
            metadata: HashMap::new(),
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_start_bot_with_metadata() {
        let mut metadata = HashMap::new();
        metadata.insert("app_secret".into(), "deadbeef".into());
        metadata.insert("base_url".into(), "https://open.feishu.cn".into());
        let frame = Frame::StartBot {
            bot_id: "lark-bot".into(),
            token: "app_id_token".into(),
            metadata,
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn start_bot_metadata_omitted_when_empty() {
        let frame = Frame::StartBot {
            bot_id: "prod-bot".into(),
            token: "123:ABC".into(),
            metadata: HashMap::new(),
        };
        let bytes = encode(&frame).unwrap();
        let as_str = String::from_utf8_lossy(&bytes);
        assert!(
            !as_str.contains("metadata"),
            "metadata key should be omitted when empty",
        );
        assert_eq!(frame, decode(&bytes).unwrap());
    }

    #[test]
    fn round_trip_secret_request() {
        let frame = Frame::SecretRequest {
            request_id: "req-1".into(),
            bot_id: "lark-bot".into(),
            op: SecretOp::Set,
            key: Some("uat/userA".into()),
            value: Some("ya29...".into()),
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_secret_reply_get() {
        let frame = Frame::SecretReply {
            request_id: "req-1".into(),
            ok: true,
            value: Some("ya29...".into()),
            keys: None,
            error: None,
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_secret_reply_list() {
        let frame = Frame::SecretReply {
            request_id: "req-2".into(),
            ok: true,
            value: None,
            keys: Some(vec!["uat/a".into(), "uat/b".into()]),
            error: None,
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_secret_reply_err() {
        let frame = Frame::SecretReply {
            request_id: "req-3".into(),
            ok: false,
            value: None,
            keys: None,
            error: Some("bot_unknown".into()),
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
