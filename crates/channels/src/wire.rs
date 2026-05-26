//! Wire protocol shared by the gateway channel server and every
//! channel client.
//!
//! After the Channel / Connection / Subscription refactor a Register
//! frame only names the channel a connection serves; per-session
//! routing is expressed by explicit `Subscribe` / `Unsubscribe` frames
//! on `ChannelKind::Subscribed` channels. `ChannelKind::Multiplexed`
//! channels' connections (telegram, weixin, …) skip subscriptions
//! entirely and see every session of their channel type.
//!
//! Consumers: the TypeScript SDK at `sdks/channel-ts/`, the built-in
//! TUI's WS client, and (forthcoming) the web chat page. All speak the
//! types below verbatim, both encode/decode via MessagePack with named
//! fields.

use aura_model::{ChannelType, ResourceAccess, SessionId};
use aura_tools::ApprovalDecision;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::types::MessageRole;

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

/// Source side of a [`Frame::SessionActivity`] event. Lets clients
/// render kind-specific UI hints (e.g. "you typed in another tab" vs
/// "agent replied") without inspecting any other frame.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sdks/channel-ts/src/generated/")
)]
pub enum ActivityKind {
    /// A user message landed on the session — either typed in another
    /// tab of the same operator, or arrived via a non-http channel
    /// (telegram/weixin) that the operator also watches.
    User,
    /// The agent emitted toward the session: streaming `AnswerDelta`, a
    /// final `Message`, or a `Notice`. First AnswerDelta of a stream is the
    /// "agent started responding" signal; throttling collapses the
    /// rest.
    Assistant,
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

/// The canonical user-visible message in either direction. A single
/// connection may carry messages for many `user_id`s — broadcast
/// channels (telegram / weixin) multiplex platform users onto one
/// WebSocket.
///
/// `role` disambiguates direction inside the wire: `User` for inbound
/// or the server's echo of inbound, `Assistant` for agent output.
/// Defaults to `Assistant` so a hypothetical decoder that misses the
/// field still renders an agent reply (the historical assumption).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sdks/channel-ts/src/generated/")
)]
pub struct Message {
    pub content: String,
    #[cfg_attr(feature = "ts-export", ts(type = "string"))]
    pub session_id: SessionId,
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
    /// care to dedup) leave it empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub platform_msg_id: String,
    /// Who said it. Server-echo of inbound carries `User`; agent output
    /// carries `Assistant`.
    #[serde(default)]
    pub role: MessageRole,
    /// Persisted `session_messages.ordinal` of this row, when known.
    /// Server-side **catch-up replays** (emitted in response to a
    /// `Subscribe { since_ordinal }`) set this so clients can advance
    /// their cursor; live emissions (inbound echo, agent reply at
    /// emit-time) leave it `None` because persistence happens out of
    /// band from the channel fan-out. Clients track the highest
    /// `Some(ordinal)` they've ever seen per `session_id` and replay it
    /// on the next Subscribe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub ordinal: Option<i64>,
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
    /// First frame after the WebSocket handshake. Names the channel
    /// this connection serves. `token` is the connection's capability
    /// token (injected via `AURA_CHANNEL_TOKEN`); for the built-in TUI
    /// the field is left empty because the channel-auth middleware
    /// already validated a vault-issued token on the upgrade request.
    ///
    /// Per-session interest is expressed by subsequent `Subscribe`
    /// frames, not by the Register frame. `Multiplexed`-kind channels
    /// (telegram, weixin) auto-subscribe to every session of their
    /// type and ignore Subscribe entirely.
    Register {
        token: String,
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        channel_type: ChannelType,
    },
    /// Server response to `Register`. `ok: false` carries a
    /// human-readable reason.
    RegisterAck { ok: bool, reason: Option<String> },
    /// Client → server. Subscribe this connection to `session_id`.
    /// Server returns no per-Subscribe ack — the connection sees
    /// outbound frames for that session start arriving. `Subscribed`-
    /// kind only; `Multiplexed` clients sending Subscribe receive a
    /// `Notice` (level=`"error"`) and the frame is dropped.
    ///
    /// `since_ordinal` is the highest `session_messages.ordinal` the
    /// client has already seen for this session; on `Some(n)` the
    /// server replays every persisted UI-visible row whose ordinal is
    /// strictly greater than `n` as `Frame::Message` (with `ordinal`
    /// set) to **this connection only**, so a tab that briefly lost
    /// the WS doesn't have to refetch via REST to recover messages
    /// that arrived during the gap. `None` is "fresh subscribe — no
    /// catch-up". If the catch-up slice would exceed the server's
    /// safety cap, the server sends a `Reset` instead so the client
    /// falls back to a paged REST fetch.
    Subscribe {
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts-export", ts(optional))]
        since_ordinal: Option<i64>,
    },
    /// Client → server. Drop one subscription. Idempotent (no error if
    /// the connection wasn't subscribed).
    Unsubscribe {
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        session_id: SessionId,
    },
    /// Server → client. The connection's live stream is in an
    /// indeterminate state (slow-consumer drop, server-side
    /// reconfiguration, etc.); clients should re-subscribe and refetch
    /// session history via the REST `/v1/chat/sessions/:id/history`
    /// endpoint. Sent best-effort; clients that ignore it may end up
    /// with a stale transcript until the next reconnect.
    Reset { reason: String },
    /// A user-visible message flowing in either direction. Inbound it
    /// is user input (role=User); outbound it is either the agent's
    /// final response for a turn (role=Assistant) or the server's
    /// echo of inbound to other subscribers of the same session
    /// (role=User).
    Message(Message),
    /// Server → client: incremental assistant **answer** text chunk for
    /// the in-flight response on a session (the reply prose — distinct
    /// from `Reasoning`, the thinking trace). Channels without a partial
    /// surface may drop this. `user_id` mirrors the Message frame so
    /// sidecars that route outbound by platform user (Telegram chat,
    /// Discord DM) don't need a `session_id → user` reverse map.
    /// Empty string for non-user-addressed emissions (cron, system).
    AnswerDelta {
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        user_id: String,
        text: String,
    },
    /// Server → client: incremental model reasoning ("thinking") chunk
    /// for the in-flight response. Rendered dim/collapsible; channels
    /// without a reasoning surface drop it. `user_id` mirrors `AnswerDelta` —
    /// empty string for non-user-addressed emissions (cron, system).
    Reasoning {
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        user_id: String,
        text: String,
    },
    /// Server → client: a tool call started. Clients render it as a live
    /// work-progress line; `label` is a human preview (falling back to
    /// `tool` when absent) and `call_id` pairs it with the later
    /// `ToolCompleted`. Channels without a progress surface drop it.
    ToolStarted {
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        user_id: String,
        call_id: String,
        tool: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts-export", ts(optional))]
        label: Option<String>,
    },
    /// Server → client: a tool call finished. `status` is a lower-case
    /// string (`"ok"` / `"error"` / `"denied"`) like `Notice.level`, so
    /// third-party clients don't need a typed enum; `summary` is a short
    /// result rendering. Pairs with `ToolStarted` by `call_id`.
    ToolCompleted {
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        user_id: String,
        call_id: String,
        status: String,
        summary: String,
    },
    /// Server → client: out-of-band notice surfaced by the agent
    /// (skill warnings, degraded-mode banners). `level` is a lower-
    /// case string (`"warn"` / `"error"`) so third-party clients don't
    /// need a typed enum to render it.
    Notice {
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        user_id: String,
        level: String,
        text: String,
    },
    /// Server → client: a tool call is blocked waiting for the
    /// channel's user to approve or deny. Clients with an approval UX
    /// should echo a [`Frame::ResolveApproval`] back; clients without
    /// one can ignore, and the gate will time out server-side.
    ApprovalRequested {
        call_id: String,
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        user_id: String,
        tool: String,
        accesses: Vec<ResourceAccess>,
        params_preview: String,
        /// Optional human-readable label the tool produced via
        /// `Tool::call_label` (e.g. Bash's `description` parameter).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    /// Server → client: any subscriber (including ours) resolved the
    /// approval — drop the prompt from the local UI so concurrent
    /// frontends stay consistent.
    ApprovalResolved {
        call_id: String,
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        decision: ApprovalDecision,
    },
    /// Client → server: resolve a pending approval the client
    /// previously saw in an [`Frame::ApprovalRequested`].
    ResolveApproval {
        call_id: String,
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        decision: ApprovalDecision,
    },
    /// Server → client: authoritative list of currently-pending approval
    /// `call_id`s for `session_id`, sent once per [`Frame::Subscribe`]
    /// right after the subscription registers. Clients use this to
    /// reconcile locally-cached [`Frame::ApprovalRequested`] cards
    /// against the server's truth: a card whose `call_id` is absent
    /// from the snapshot was resolved while the connection was down
    /// (the resulting [`Frame::ApprovalResolved`] is fire-and-forget
    /// fan-out — not persisted, not replayed on catch-up). Empty
    /// `call_ids` is a meaningful "nothing pending here" — not a
    /// "no opinion" sentinel.
    ///
    /// Race note: an approval enqueued on the server between the
    /// subscribe registration and the snapshot's queue read may
    /// neither appear in `call_ids` nor have been broadcast through
    /// this connection yet. The client side is expected to guard
    /// against dropping locally-cached entries that arrived *after*
    /// the subscribe-issuance moment.
    PendingApprovalsSnapshot {
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        session_id: SessionId,
        call_ids: Vec<String>,
    },
    /// Client → server: persist one submitted input line to the
    /// server-side history store. Used by the built-in TUI to get
    /// zsh-style history without the client holding any encryption key
    /// itself — the gateway's [`aura_security::SecretVault`] is the
    /// single writer. Fire-and-forget; the server does not ack
    /// per-append.
    HistoryAppend {
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        session_id: SessionId,
        entry: String,
    },
    /// Server → client: the full history ring the TUI should rehydrate
    /// its in-memory scrollback from. Sent exactly once right after
    /// `RegisterAck { ok: true }` for the TUI channel; sidecars never
    /// receive this.
    HistorySnapshot {
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        session_id: SessionId,
        entries: Vec<String>,
    },
    /// Server → client: attach a new bot (or other per-tenant
    /// credential) to a broadcast sidecar that multiplexes many
    /// tenants over one WS. For the Telegram channel `bot_id` is an
    /// operator-chosen stable label and `token` is the @BotFather
    /// token. Sidecars reply with [`Frame::BotStatus`] to ack.
    StartBot { bot_id: String, token: String },
    /// Server → client: detach a previously-attached bot. The sidecar
    /// stops polling for that bot and drops its in-process state.
    StopBot { bot_id: String },
    /// Client → server: ack for a `StartBot` / `StopBot` command.
    /// `ok: true` means the command ran; `ok: false` carries a
    /// human-readable reason so aura can surface it (e.g. bad token).
    BotStatus {
        bot_id: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts-export", ts(optional))]
        message: Option<String>,
    },
    /// Server → client: gateway-authored list of slash commands the
    /// sidecar should publish on its native command surface (e.g.
    /// Telegram's `setMyCommands`). Sent once right after a successful
    /// `RegisterAck` for broadcast channels; the manifest replaces any
    /// prior list. Sidecars that don't surface client-side autocomplete
    /// may ignore this.
    SlashManifest { commands: Vec<SlashCommandSpec> },
    /// Server → client: structural session-metadata change. Broadcast
    /// to every connection on the `http` channel regardless of
    /// subscription so every open chat tab converges on the new state
    /// without a full list refetch. Sidecars and non-web channels
    /// ignore this frame.
    ///
    /// Reserved for low-frequency, operator-driven mutations: Create,
    /// Hide, Unhide. Per-turn liveness bumps are a separate frame
    /// ([`Frame::SessionActivity`]) so this one stays sparse and
    /// cacheable.
    ///
    /// Patch semantics — see [`SessionPatch`]:
    /// * fields the producer didn't touch are absent (`None`); clients
    ///   merge present fields onto their local row and leave the rest;
    /// * a patch for a `session_id` the client doesn't yet know
    ///   constructs a new row iff it carries enough fields (currently
    ///   `created_at` + `last_active`); otherwise the patch is dropped
    ///   and the client picks the session up at next list refetch.
    ///
    /// Producers: `POST /v1/chat/sessions` (Created — full patch),
    /// `DELETE /v1/chat/sessions/:id` (hidden=true), `unhide` (full
    /// patch).
    SessionUpdated {
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        session_id: SessionId,
        patch: SessionPatch,
    },
    /// Server → client: a session just had activity (a user message
    /// landed, or the agent emitted output). Broadcast to every
    /// connection on the `http` channel regardless of subscription —
    /// that is the whole point: a sidebar tab whose operator is
    /// looking at session A still gets a cheap unread signal for
    /// session F, without having to subscribe to F and pay for the
    /// full AnswerDelta stream.
    ///
    /// Throttled at the broadcaster (see
    /// `gateway::channel::session_pulse`) to one frame per
    /// `(session_id, kind)` per ~1.5 s window. `kind` lets the client
    /// distinguish "user typed in another tab" (might already be
    /// visible elsewhere) from "agent replied" (truly inbound from
    /// the client's perspective).
    ///
    /// Receivers project `at` onto their local `last_active` for the
    /// session — that's what drives sidebar age strings and
    /// most-recent-first sort — and bump the unread badge if the
    /// session isn't currently in the foreground. Sidecars and TUI
    /// ignore this frame.
    SessionActivity {
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        session_id: SessionId,
        /// Renamed from the natural `kind` because `kind` is the
        /// Frame-level serde discriminator — a same-named variant
        /// field would collide on the wire.
        source: ActivityKind,
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        at: DateTime<Utc>,
    },
    /// Liveness probe (either direction). The receiver MUST reply with
    /// [`Frame::Pong`].
    ///
    /// Half-open WS detection: a TCP connection can stay "open" client-
    /// side after the peer goes silent (NAT idle, laptop sleep, mobile
    /// roaming) — the browser fires no `onclose`, so the chat tab keeps
    /// looking healthy while outbound sends pile into kernel buffers and
    /// silently disappear. The web chat client paces a `Ping` every
    /// ~20 s and force-closes if it hasn't seen any frame for ~45 s,
    /// which then trips the normal reconnect ladder. WS protocol-level
    /// `Ping`/`Pong` would do the same on the server side but browsers
    /// hide control-frame reception from JS, so the client-side
    /// watchdog needs an app-level signal it can actually observe.
    Ping,
    /// Reply to [`Frame::Ping`]. Carries no payload — receipt itself is
    /// the liveness signal.
    Pong,
}

/// Sparse mutation surface carried on [`Frame::SessionUpdated`].
/// Every field is independently optional; producers populate only what
/// changed. Receivers merge present fields onto their local view —
/// absent does *not* mean "set to null", it means "no change".
///
/// New session-metadata fields slot in by adding another optional
/// here; the existing producers don't have to learn about them, and
/// older clients ignore unknown keys (msgpack-named decode tolerates
/// unknown fields).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sdks/channel-ts/src/generated/")
)]
pub struct SessionPatch {
    /// Populated on Create / Unhide so a sibling tab that doesn't have
    /// the session in its local list can still construct a sidebar
    /// row without a list refetch. Stable for the lifetime of the
    /// session; bumps on touch are carried via `last_active` instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional, type = "string"))]
    pub created_at: Option<DateTime<Utc>>,
    /// Authoritative `last_active` snapshot at patch-emit time. Only
    /// carried on Create / Unhide so a sibling tab that doesn't have
    /// the row yet (or hid it earlier) can render a correct age
    /// string immediately. Per-turn liveness updates ride
    /// [`Frame::SessionActivity`] instead, which the client projects
    /// onto its local `last_active`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional, type = "string"))]
    pub last_active: Option<DateTime<Utc>>,
    /// Flipped by `DELETE /v1/chat/sessions/:id` (true) and `unhide`
    /// (false). `true` means remove from sidebar; `false` paired with
    /// `created_at` + `last_active` lets a client re-add a previously-
    /// hidden session it might never have seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub hidden: Option<bool>,
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

    fn sample_register() -> Frame {
        Frame::Register {
            token: "deadbeef".into(),
            channel_type: ChannelType::from("slack"),
        }
    }

    #[test]
    fn round_trip_register() {
        let frame = sample_register();
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_subscribe() {
        let frame = Frame::Subscribe {
            session_id: "sess-x".into(),
            since_ordinal: None,
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_subscribe_with_cursor() {
        let frame = Frame::Subscribe {
            session_id: "sess-x".into(),
            since_ordinal: Some(42),
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_unsubscribe() {
        let frame = Frame::Unsubscribe {
            session_id: "sess-x".into(),
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_reset() {
        let frame = Frame::Reset {
            reason: "outbound queue full".into(),
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_message_with_role() {
        let frame = Frame::Message(Message {
            content: "hi".into(),
            session_id: "s1".into(),
            user_id: "u1".into(),
            channel_type: ChannelType::from("http"),
            bot_id: String::new(),
            attachments: Vec::new(),
            platform_msg_id: String::new(),
            role: MessageRole::User,
            ordinal: None,
        });
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_message_with_ordinal_cursor() {
        let frame = Frame::Message(Message {
            content: "hi".into(),
            session_id: "s1".into(),
            user_id: "u1".into(),
            channel_type: ChannelType::from("http"),
            bot_id: String::new(),
            attachments: Vec::new(),
            platform_msg_id: String::new(),
            role: MessageRole::Assistant,
            ordinal: Some(7),
        });
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn message_role_defaults_to_assistant_when_field_missing() {
        // A producer that omits `role` (e.g. tests written against an
        // earlier draft) decodes as Assistant.
        let frame = Frame::Message(Message {
            content: "hi".into(),
            session_id: "s1".into(),
            user_id: "u1".into(),
            channel_type: ChannelType::from("slack"),
            bot_id: String::new(),
            attachments: Vec::new(),
            platform_msg_id: String::new(),
            role: MessageRole::Assistant,
            ordinal: None,
        });
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
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
        let frame = Frame::AnswerDelta {
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
    fn round_trip_ping() {
        let frame = Frame::Ping;
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_pong() {
        let frame = Frame::Pong;
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_session_updated_full_patch() {
        let now = chrono::Utc::now();
        let frame = Frame::SessionUpdated {
            session_id: "sess-abc".into(),
            patch: SessionPatch {
                created_at: Some(now),
                last_active: Some(now),
                hidden: Some(false),
            },
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_session_updated_sparse_patch() {
        // A `last_active`-only patch — the per-turn broadcaster shape.
        // Absent fields stay absent on decode (None), so older clients
        // that don't know about future fields still see a clean merge.
        let now = chrono::Utc::now();
        let frame = Frame::SessionUpdated {
            session_id: "sess-abc".into(),
            patch: SessionPatch {
                created_at: None,
                last_active: Some(now),
                hidden: None,
            },
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_session_updated_hidden_only() {
        let frame = Frame::SessionUpdated {
            session_id: "sess-abc".into(),
            patch: SessionPatch {
                hidden: Some(true),
                ..SessionPatch::default()
            },
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_session_activity_user() {
        let now = chrono::Utc::now();
        let frame = Frame::SessionActivity {
            session_id: "sess-abc".into(),
            source: ActivityKind::User,
            at: now,
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_session_activity_assistant() {
        let now = chrono::Utc::now();
        let frame = Frame::SessionActivity {
            session_id: "sess-abc".into(),
            source: ActivityKind::Assistant,
            at: now,
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }
}
