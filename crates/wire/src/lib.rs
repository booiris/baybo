//! Wire protocol shared by the gateway channel server and every
//! channel client.
//!
//! These are the **pure wire types** — `Frame`, `Message`, `MessageRole`,
//! the attachment / folder / task projections — plus the MessagePack codec.
//! They were extracted from `baybo-channels` into their own crate so the device
//! companion core can speak the exact same protocol without
//! pulling `baybo-channels → baybo-tools → { libsql, axum, reqwest, … }`, a
//! chain that cannot cross-compile to iOS. `baybo-channels` re-exports this
//! crate as its `wire` module, so server-side consumers are unchanged.
//!
//! After the Channel / Connection / Subscription refactor a Register
//! frame only names the channel a connection serves; per-session
//! routing is expressed by explicit `Subscribe` / `Unsubscribe` frames
//! on `ChannelKind::Subscribed` channels. `ChannelKind::Multiplexed`
//! channels' connections (telegram, weixin, …) skip subscriptions
//! entirely and see every session of their channel type.
//!
//! Consumers: the TypeScript SDK at `sidecars/sdk/channel-ts/`, the built-in
//! TUI's WS client, the web chat page, and device companion clients. All speak the
//! types below verbatim, both encode/decode via MessagePack with named
//! fields.

use baybo_model::{ApprovalDecision, ChannelType, ResourceAccess, SessionId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Role discriminator on a [`Message`].
///
/// In-tree producers always set this explicitly: the agent emits
/// `Assistant`, sidecars and inbound echo emit `User`. Default is
/// `Assistant` so a wire frame that omits the field decodes as an
/// agent reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/sdk/channel-ts/src/generated/")
)]
pub enum MessageRole {
    User,
    #[default]
    Assistant,
}

/// Error surface for frame encode/decode.
#[derive(Debug, Error)]
pub enum WireError {
    #[error("encode: {0}")]
    Encode(#[from] rmp_serde::encode::Error),

    #[error("decode: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
}

/// Discriminator on a [`WireAttachment`]. Maps 1:1 to the matching
/// [`baybo_model::ContentBlock`] variant on either side of the bridge.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/sdk/channel-ts/src/generated/")
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
    ts(export, export_to = "../../../sidecars/sdk/channel-ts/src/generated/")
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
    ts(export, export_to = "../../../sidecars/sdk/channel-ts/src/generated/")
)]
pub struct WireAttachment {
    pub kind: AttachmentKind,
    /// Capability id from the `BlobStore` (`"sha256:<64hex>.<read_token>"`).
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
    ts(export, export_to = "../../../sidecars/sdk/channel-ts/src/generated/")
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
    /// The final assistant reply carries its persisted ordinal so
    /// clients can advance their sync cursor past live emissions; the
    /// server's pre-persist echo of inbound (role=User) leaves it
    /// `None` by design — durability for a send is confirmed by an
    /// ordinal-stamped row from the REST sync/backfill surface, keyed
    /// by `platform_msg_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub ordinal: Option<i64>,
}

/// Maximum number of user messages allowed in one inbound [`Frame::Messages`]
/// batch. Gateway validation and agent-side defense-in-depth share this so the
/// WS boundary and router contract cannot drift.
pub const MAX_MESSAGE_BATCH_MESSAGES: usize = 64;

/// Maximum aggregate UTF-8 text bytes allowed across one inbound
/// [`Frame::Messages`] batch.
pub const MAX_MESSAGE_BATCH_TEXT_BYTES: usize = 64 * 1024;

/// Maximum aggregate attachments allowed across one inbound [`Frame::Messages`]
/// batch. Attachments carry blob IDs, not bytes; this caps fan-out and downstream
/// per-message conversion work.
pub const MAX_MESSAGE_BATCH_ATTACHMENTS: usize = 64;

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
    ts(export, export_to = "../../../sidecars/sdk/channel-ts/src/generated/")
)]
pub struct SlashCommandSpec {
    pub command: String,
    pub description: String,
}

/// One chat-list folder on the wire — a presentation projection of a
/// session folder for the web sidebar. Carried in the full-snapshot
/// [`Frame::FoldersChanged`]. `parent_id` is `null`/absent for a
/// top-level folder.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/sdk/channel-ts/src/generated/")
)]
pub struct FolderView {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub parent_id: Option<String>,
    pub name: String,
    pub position: i64,
    #[cfg_attr(feature = "ts-export", ts(type = "string"))]
    pub created_at: DateTime<Utc>,
}

/// A folder reassignment carried on [`SessionPatch::folder_id`]. Modelled
/// as an explicit two-state enum (NOT `Option<Option<String>>`) so the
/// MessagePack + ts-rs round-trip cleanly distinguishes the two meanings
/// from "field absent" (= no change): `Set` moves the session into a
/// folder, `Uncategorized` clears the assignment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/sdk/channel-ts/src/generated/")
)]
pub enum FolderChange {
    Set { id: String },
    Uncategorized,
}

/// One task on the wire — a flattened, presentation-only projection of a
/// `session_tasks` row for the web checklist. `subject` is the title shown in
/// the list (the `description` body stays server-side). `status` is a
/// lower-case string (`"pending"` / `"in_progress"` / `"completed"`) so
/// third-party clients don't need a typed enum.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/sdk/channel-ts/src/generated/")
)]
pub struct TaskView {
    pub id: String,
    pub subject: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
}

impl From<baybo_model::Task> for TaskView {
    fn from(task: baybo_model::Task) -> Self {
        Self {
            id: task.id.to_string(),
            subject: task.subject,
            status: task.status.as_str().to_string(),
            depends_on: task.depends_on.iter().map(|d| d.to_string()).collect(),
        }
    }
}

/// Kind discriminant for a [`WireWorkStep`] — serialized as `"reasoning"` /
/// `"prose"` / `"tool"`. A typed enum (mirrors [`AttachmentKind`]) so the
/// discriminant round-trips cleanly through ts-rs.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/sdk/channel-ts/src/generated/")
)]
pub enum WireWorkStepKind {
    /// A model reasoning ("thinking") chunk.
    Reasoning,
    /// Answer prose that streamed mid-turn but was superseded by more work.
    Prose,
    /// A tool call (started, and — if it finished within the buffered turn —
    /// completed).
    Tool,
}

/// One step inside a turn's in-flight work block, carried in the
/// [`Frame::SubscribeState`] bundle. Mirrors the client's rendered work-step model:
/// `reasoning` / `prose` bodies live in [`Self::text`]; a `tool` step carries
/// the call's `call_id` (so a later live [`Frame::ToolCompleted`] still pairs by
/// id), a display `tool` name + optional `label`, and — once the call finished
/// within the buffered turn — a `status` (`"ok"` / `"error"` / `"denied"`, else
/// the tool is still running) and a `summary`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/sdk/channel-ts/src/generated/")
)]
pub struct WireWorkStep {
    pub kind: WireWorkStepKind,
    /// Reasoning trace or superseded prose body. Empty for `tool` steps.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// Pairs a `tool` step with a later live [`Frame::ToolCompleted`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub call_id: Option<String>,
    /// Tool name, set when `kind == Tool`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub tool: Option<String>,
    /// Human preview for the call (path / command / url), when the live
    /// `progress_label` was present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub label: Option<String>,
    /// `"ok"` / `"error"` / `"denied"` once the call completed within the
    /// buffered turn; `None` while it is still running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub status: Option<String>,
    /// Short result summary, set alongside `status` on completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub summary: Option<String>,
}

impl WireWorkStep {
    /// A reasoning ("thinking") step.
    pub fn reasoning(text: String) -> Self {
        Self {
            kind: WireWorkStepKind::Reasoning,
            text,
            call_id: None,
            tool: None,
            label: None,
            status: None,
            summary: None,
        }
    }

    /// A superseded mid-turn answer-prose step.
    pub fn prose(text: String) -> Self {
        Self {
            kind: WireWorkStepKind::Prose,
            text,
            call_id: None,
            tool: None,
            label: None,
            status: None,
            summary: None,
        }
    }

    /// A tool step (status / summary filled in later on completion). `call_id`
    /// is `Some` for a still-running in-flight tool (so a later live
    /// `ToolCompleted` can pair by id) and `None` for a completed-turn replay
    /// (nothing live to pair with).
    pub fn tool(call_id: Option<String>, tool: Option<String>, label: Option<String>) -> Self {
        Self {
            kind: WireWorkStepKind::Tool,
            text: String::new(),
            call_id,
            tool,
            label,
            status: None,
            summary: None,
        }
    }
}

/// Whether a turn (the session's in-flight reply) is being produced at
/// snapshot time, carried in the [`Frame::SubscribeState`] bundle.
/// `started_at` is `Some` iff `active` — it doubles as the turn's
/// identity for the client-side staleness test: the snapshot's
/// turn/work halves are discarded only when the client already holds a
/// turn-end signal for the *same* turn, matched by `started_at`, never
/// by comparing a sync cursor against `as_of_ordinal` (the cursor
/// advances mid-turn as tool rows persist).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/sdk/channel-ts/src/generated/")
)]
pub struct TurnSnapshot {
    pub active: bool,
    /// `Some` iff `active`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional, type = "string"))]
    pub started_at: Option<DateTime<Utc>>,
}

/// One pending tool-approval prompt, carried in the
/// [`Frame::SubscribeState`] bundle as part of the authoritative
/// replacement set for the session. Field-compatible with the live
/// [`Frame::ApprovalRequested`] (minus `session_id`, which the bundle
/// carries once) so clients render both through the same card.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/sdk/channel-ts/src/generated/")
)]
pub struct ApprovalCard {
    pub call_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user_id: String,
    pub tool: String,
    pub accesses: Vec<ResourceAccess>,
    pub params_preview: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub description: Option<String>,
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
    ts(export, export_to = "../../../sidecars/sdk/channel-ts/src/generated/")
)]
pub enum Frame {
    /// First frame after the WebSocket handshake. Names the channel
    /// this connection serves. `token` is the connection's capability
    /// token (injected via `BAYBO_CHANNEL_TOKEN`); for the built-in TUI
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
    /// outbound frames for that session start arriving, led by one
    /// [`Frame::SubscribeState`] bundle. `Subscribed`-kind only;
    /// `Multiplexed` clients sending Subscribe receive a `Notice`
    /// (level=`"error"`) and the frame is dropped.
    ///
    /// The server replays no history on subscribe — transcript
    /// recovery is the client's REST sync call
    /// (`GET /v1/chat/sessions/{id}/sync`), the one forward-recovery
    /// pull for every scenario.
    Subscribe {
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        session_id: SessionId,
    },
    /// Client → server. Drop one subscription. Idempotent (no error if
    /// the connection wasn't subscribed).
    Unsubscribe {
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        session_id: SessionId,
    },
    /// Server → client: the one atomic state-plane bundle, sent
    /// immediately after a `Subscribe` registers. REPLACES the client's
    /// prior view of the session's current state: whether a turn is in
    /// flight (`turn`), the in-flight work block streamed so far
    /// (`work_steps`, empty unless mid-turn), the authoritative pending
    /// approval set (full cards from one atomic queue read), and the
    /// planning checklist (`tasks`). `as_of_ordinal` is the session's
    /// newest persisted ordinal at snapshot time — it orders the bundle
    /// against sync pages, but staleness of the turn/work halves is
    /// judged by turn identity ([`TurnSnapshot::started_at`]), never by
    /// ordinal arithmetic. Live `TurnState` / `TaskList` /
    /// `ApprovalRequested` / `ApprovalResolved` frames arriving after
    /// this bundle win by normal frame order.
    SubscribeState {
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        session_id: SessionId,
        /// Newest persisted `session_messages.ordinal` at snapshot
        /// time; `None` for a session with no rows yet.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts-export", ts(optional))]
        as_of_ordinal: Option<i64>,
        turn: TurnSnapshot,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        work_steps: Vec<WireWorkStep>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pending_approvals: Vec<ApprovalCard>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tasks: Vec<TaskView>,
    },
    /// Server → client: the server dropped one or more frames it owed
    /// this connection (slow-consumer drop on the bounded per-connection
    /// queue). `Some(id)` scopes the loss to one session — the client
    /// runs sync for it. `None` means the loss couldn't be attributed
    /// to a session (a dropped session-less broadcast such as
    /// `SessionActivity` / `SessionUpdated` / `FoldersChanged`) — the
    /// client syncs every subscribed session and refetches the session
    /// list + folders. Best-effort by construction (it rides the same
    /// queue it reports on); the client-side periodic safety-net sync
    /// backstops a lost Gap.
    Gap {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts-export", ts(optional, type = "string"))]
        session_id: Option<SessionId>,
    },
    /// A user-visible message flowing in either direction. Inbound it
    /// is user input (role=User); outbound it is either the agent's
    /// final response for a turn (role=Assistant) or the server's
    /// echo of inbound to other subscribers of the same session
    /// (role=User).
    Message(Message),
    /// Client → server. Several user messages for **one** session that
    /// must run as a single coalesced turn — the web "send every queued
    /// message at once" affordance. Delivered to the actor atomically (as
    /// one mailbox item) so the actor's coalescing can't lose stragglers to
    /// the per-message router latency, which a burst of separate
    /// [`Frame::Message`]s would. The server echoes each entry back as its
    /// own [`Frame::Message`] (role=User) so every subscriber renders the
    /// same N rows, then runs one merged turn with one reply. Outbound never
    /// uses this kind.
    Messages { messages: Vec<Message> },
    /// Server → client: media a tool produced mid-turn (a sent file, a
    /// screenshot), delivered as its **own** message. Unlike a terminal
    /// [`Frame::Message`] it carries no `role` / `ordinal` and no turn-
    /// completion meaning: clients render it as a standalone bubble
    /// (or, for sidecars, a standalone platform send) and must NOT fold
    /// it into — or close — an in-flight turn's work block. Sidecars
    /// deliver the attachments exactly as they would a `Message`'s;
    /// surfaces that can't render media drop it. `user_id` mirrors
    /// `Message` so sidecars route by platform user without a reverse map.
    Attachment {
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        user_id: String,
        attachments: Vec<WireAttachment>,
    },
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
    /// Server → client: a coarse turn-phase transition for a transient
    /// status line. `phase` is a lower-case string (today `"compacting"` /
    /// `"compacted"` for context compaction start/end); clients show a
    /// spinner/banner on the start and clear it on the matching end, and
    /// surfaces without one drop it.
    Status {
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        user_id: String,
        phase: String,
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
        /// `true` for a *transient* mid-turn progress update (the progress
        /// observer) rather than a terminal notice. Clients that render a
        /// per-turn work block must fold it into the open block and keep
        /// the turn going; clients without one render it like any notice.
        /// Omitted on the wire when `false` (the default), so existing
        /// terminal notices are byte-identical.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        transient: bool,
    },
    /// Server → client: the session's full planning checklist after it changed
    /// (a `Task*` call) or at turn start. An idempotent snapshot
    /// that REPLACES the client's prior list for this session — not a delta.
    /// Empty `tasks` means the list is currently empty. Clients without a
    /// checklist surface drop it.
    TaskList {
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        user_id: String,
        tasks: Vec<TaskView>,
    },
    /// Server → client: whether a turn (the session's in-flight reply)
    /// is currently being produced. An idempotent snapshot, recomputed
    /// from the job store and broadcast by the turn-state projector on
    /// every job lifecycle transition — the `Pending → InProgress` start
    /// edge (`active: true` + start instant) and the terminal edges
    /// (`active: false`, so the close can't be skipped by an error or
    /// crash). A late joiner (new tab, reconnect) learns the in-flight
    /// turn from the [`Frame::SubscribeState`] bundle instead — this
    /// frame is live-update only. Clients that infer turn activity
    /// locally (TUI, sidecars) drop it.
    TurnState {
        #[cfg_attr(feature = "ts-export", ts(type = "string"))]
        session_id: SessionId,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        user_id: String,
        active: bool,
        /// `Some` iff `active`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts-export", ts(optional, type = "string"))]
        started_at: Option<DateTime<Utc>>,
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
    /// Client → server: persist one submitted input line to the
    /// server-side history store. Used by the built-in TUI to get
    /// zsh-style history without the client holding any encryption key
    /// itself — the gateway's [`baybo_security::SecretVault`] is the
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
    /// human-readable reason so baybo can surface it (e.g. bad token).
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
    /// Server → client: the chat-list folder tree changed (create /
    /// rename / delete / reorder / reparent). Broadcast to every
    /// `http` connection as a full snapshot — clients replace their local
    /// folder list wholesale, no patch-merge. Folders are few, so the
    /// snapshot is tiny. Per-session `folder_id` reassignment rides
    /// [`SessionPatch`] instead. Sidecars and non-web channels ignore it.
    FoldersChanged { folders: Vec<FolderView> },
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

impl Frame {
    /// Session id a client can use to route this frame into a per-session bucket.
    /// Returns `None` for connection-global frames. `Messages` returns the first
    /// message's session id; empty batches route nowhere.
    pub fn routing_session_id(&self) -> Option<&SessionId> {
        match self {
            Frame::Subscribe { session_id }
            | Frame::Unsubscribe { session_id }
            | Frame::SubscribeState { session_id, .. }
            | Frame::Attachment { session_id, .. }
            | Frame::AnswerDelta { session_id, .. }
            | Frame::Reasoning { session_id, .. }
            | Frame::ToolStarted { session_id, .. }
            | Frame::ToolCompleted { session_id, .. }
            | Frame::Status { session_id, .. }
            | Frame::Notice { session_id, .. }
            | Frame::TaskList { session_id, .. }
            | Frame::TurnState { session_id, .. }
            | Frame::ApprovalRequested { session_id, .. }
            | Frame::HistoryAppend { session_id, .. }
            | Frame::HistorySnapshot { session_id, .. }
            | Frame::SessionUpdated { session_id, .. }
            | Frame::SessionActivity { session_id, .. } => Some(session_id),
            Frame::Gap { session_id } => session_id.as_ref(),
            Frame::Message(message) => Some(&message.session_id),
            Frame::Messages { messages } => messages.first().map(|message| &message.session_id),
            Frame::Register { .. }
            | Frame::RegisterAck { .. }
            | Frame::ApprovalResolved { .. }
            | Frame::ResolveApproval { .. }
            | Frame::StartBot { .. }
            | Frame::StopBot { .. }
            | Frame::BotStatus { .. }
            | Frame::SlashManifest { .. }
            | Frame::FoldersChanged { .. }
            | Frame::Ping
            | Frame::Pong => None,
        }
    }
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
    ts(export, export_to = "../../../sidecars/sdk/channel-ts/src/generated/")
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
    /// Flipped by `PUT /v1/chat/sessions/:id/pin`. `true` moves the row
    /// into the sidebar's pinned block, `false` moves it back into the
    /// regular list. Carried on Create / Unhide too so a sibling tab
    /// re-adding the row renders it in the right block immediately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub pinned: Option<bool>,
    /// Set by `PUT /v1/chat/sessions/:id/folder` (and on folder delete,
    /// which clears every direct member to `Uncategorized`). Present means
    /// "the assignment changed to this value"; absent means "no change".
    /// `Set { id }` files the session under `id`; `Uncategorized` clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional))]
    pub folder_id: Option<FolderChange>,
    /// Generated conversation title; absent means no change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional, type = "string"))]
    pub title: Option<String>,
    /// Bound agent profile id. Emitted on the create broadcast so sibling
    /// tabs can render the agent chip without a refetch; never changes later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(optional, type = "string"))]
    pub agent_id: Option<String>,
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

    fn sample_message(session_id: &str) -> Message {
        Message {
            content: "hi".into(),
            session_id: session_id.into(),
            user_id: "u1".into(),
            channel_type: ChannelType::from("http"),
            bot_id: String::new(),
            attachments: Vec::new(),
            platform_msg_id: String::new(),
            role: MessageRole::User,
            ordinal: None,
        }
    }

    #[test]
    fn routing_session_id_reads_direct_session_fields() {
        let subscribe = Frame::Subscribe {
            session_id: "sess-x".into(),
        };
        assert_eq!(
            subscribe.routing_session_id().map(|id| id.as_str()),
            Some("sess-x")
        );

        let activity = Frame::SessionActivity {
            session_id: "sess-y".into(),
            source: ActivityKind::Assistant,
            at: chrono::Utc::now(),
        };
        assert_eq!(
            activity.routing_session_id().map(|id| id.as_str()),
            Some("sess-y")
        );
    }

    #[test]
    fn routing_session_id_reads_message_payloads() {
        let message = Frame::Message(sample_message("sess-message"));
        assert_eq!(
            message.routing_session_id().map(|id| id.as_str()),
            Some("sess-message")
        );

        let batch = Frame::Messages {
            messages: vec![sample_message("sess-first"), sample_message("sess-second")],
        };
        assert_eq!(
            batch.routing_session_id().map(|id| id.as_str()),
            Some("sess-first")
        );

        let empty_batch = Frame::Messages {
            messages: Vec::new(),
        };
        assert!(empty_batch.routing_session_id().is_none());
    }

    #[test]
    fn routing_session_id_ignores_connection_global_frames() {
        assert!(Frame::Ping.routing_session_id().is_none());
        assert!(
            Frame::Gap { session_id: None }
                .routing_session_id()
                .is_none()
        );
        assert_eq!(
            Frame::Gap {
                session_id: Some("sess-g".into())
            }
            .routing_session_id()
            .map(|id| id.as_str()),
            Some("sess-g")
        );
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
    fn round_trip_gap_session_scoped_and_global() {
        let scoped = Frame::Gap {
            session_id: Some("sess-x".into()),
        };
        assert_eq!(scoped, decode(&encode(&scoped).unwrap()).unwrap());

        let global = Frame::Gap { session_id: None };
        assert_eq!(global, decode(&encode(&global).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_subscribe_state_mid_turn() {
        use std::path::PathBuf;
        let frame = Frame::SubscribeState {
            session_id: "sess-x".into(),
            as_of_ordinal: Some(41),
            turn: TurnSnapshot {
                active: true,
                started_at: Some(chrono::Utc::now()),
            },
            work_steps: vec![
                WireWorkStep::reasoning("weighing it".into()),
                WireWorkStep::tool(Some("c1".into()), Some("edit".into()), None),
            ],
            pending_approvals: vec![ApprovalCard {
                call_id: "c1".into(),
                user_id: "u1".into(),
                tool: "fs.read".into(),
                accesses: vec![ResourceAccess::ReadFile {
                    path: PathBuf::from("/tmp/x"),
                }],
                params_preview: "{}".into(),
                description: None,
            }],
            tasks: vec![TaskView {
                id: "t1".into(),
                subject: "do it".into(),
                status: "pending".into(),
                depends_on: Vec::new(),
            }],
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_subscribe_state_idle_empty() {
        // The idle shape: no turn, nothing pending, no rows yet. Empty
        // vecs are skipped on the wire and must decode back to empty.
        let frame = Frame::SubscribeState {
            session_id: "sess-x".into(),
            as_of_ordinal: None,
            turn: TurnSnapshot {
                active: false,
                started_at: None,
            },
            work_steps: Vec::new(),
            pending_approvals: Vec::new(),
            tasks: Vec::new(),
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
            transient: false,
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_transient_notice() {
        let frame = Frame::Notice {
            session_id: "s1".into(),
            user_id: "u1".into(),
            level: "info".into(),
            text: "still working on it".into(),
            transient: true,
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
                pinned: Some(false),
                folder_id: Some(FolderChange::Set { id: "f1".into() }),
                title: Some("Reset password flow".into()),
                agent_id: Some("01ARZ3NDEKTSV4RRFFQ69G5FAV".into()),
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
                pinned: None,
                folder_id: None,
                title: None,
                agent_id: None,
            },
        };
        assert_eq!(frame, decode(&encode(&frame).unwrap()).unwrap());
    }

    #[test]
    fn round_trip_session_updated_folder_change() {
        // Both arms of the three-state folder field survive the msgpack
        // round-trip, distinct from "absent" (no change).
        let to_folder = Frame::SessionUpdated {
            session_id: "sess-abc".into(),
            patch: SessionPatch {
                folder_id: Some(FolderChange::Set { id: "work".into() }),
                ..SessionPatch::default()
            },
        };
        assert_eq!(to_folder, decode(&encode(&to_folder).unwrap()).unwrap());

        let uncategorized = Frame::SessionUpdated {
            session_id: "sess-abc".into(),
            patch: SessionPatch {
                folder_id: Some(FolderChange::Uncategorized),
                ..SessionPatch::default()
            },
        };
        assert_eq!(
            uncategorized,
            decode(&encode(&uncategorized).unwrap()).unwrap()
        );

        // Absent folder_id decodes back to None (no change).
        let no_change = Frame::SessionUpdated {
            session_id: "sess-abc".into(),
            patch: SessionPatch {
                pinned: Some(true),
                ..SessionPatch::default()
            },
        };
        let decoded = decode(&encode(&no_change).unwrap()).unwrap();
        assert_eq!(no_change, decoded);
        if let Frame::SessionUpdated { patch, .. } = decoded {
            assert!(patch.folder_id.is_none());
        }
    }

    #[test]
    fn round_trip_folders_changed() {
        let now = chrono::Utc::now();
        let frame = Frame::FoldersChanged {
            folders: vec![
                FolderView {
                    id: "p".into(),
                    parent_id: None,
                    name: "Parent".into(),
                    position: 0,
                    created_at: now,
                },
                FolderView {
                    id: "c".into(),
                    parent_id: Some("p".into()),
                    name: "Child".into(),
                    position: 0,
                    created_at: now,
                },
            ],
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
