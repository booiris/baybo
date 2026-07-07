//! `/v1/chat/*` — admin-side endpoints for the web chat page.
//!
//! Surface:
//!
//! * `POST /v1/chat/sessions` — create a new session (web defaults to
//!   channel=http; direct device requests carrying `x-baybo-device-id` use
//!   channel=device), return its id.
//! * `GET /v1/chat/sessions` — list the request identity's chat sessions
//!   (newest first). Hidden sessions are filtered out unless the
//!   `include_hidden=true` query is set.
//! * `GET /v1/chat/sessions/:id` — session detail + transcript history
//!   (backward paging / backfill via `before_ordinal`).
//! * `GET /v1/chat/sessions/:id/sync` — the one forward-recovery pull:
//!   full-fidelity transcript rows after a cursor (or the newest-page
//!   baseline when the cursor is absent), with rebase semantics when the
//!   difference would exceed the requested limit.
//! * `GET /v1/chat/sessions/:id/messages?platform_msg_id=…` — per-send
//!   durability point lookup for the client outbox.
//! * `DELETE /v1/chat/sessions/:id` — **hide** the session from the
//!   chat list. The row and transcript stay live; admin / trace
//!   surfaces still see it. Reversible via
//!   `POST /v1/chat/sessions/:id/unhide`.
//! * `POST /v1/chat/sessions/:id/unhide` — undo the hide.
//! * `PUT /v1/chat/sessions/:id/pin` — pin (or unpin) the session to
//!   the top of the chat list. Presentation only; the row is otherwise
//!   unchanged.
//! * `GET /v1/chat/slash-manifest` — list of slash commands the input
//!   composer's `/`-autocomplete should surface.
//!
//! The web client uses the admin bearer to authenticate against
//! `/v1/channel-ws`, which the admin listener co-hosts on its public bind
//! so the browser can reach it from the same origin that served the web
//! bundle.

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{FromRequest, Path, Query, Request, State};
use axum::{Extension, Json};
use baybo_agent::actor::AgentMessage;
use baybo_channels::wire::{
    AttachmentKind, FolderChange, FolderView, SessionPatch, SlashCommandSpec, WireAttachment,
    WireWorkStep, WireWorkStepKind,
};
use baybo_channels::{STOP_CANCELLED_REPLY_LINE, STOP_COMMAND_NAME, SessionEvent};
use baybo_model::{
    ChannelType, ChatMessage, ContentBlock, ControlEvent, ControlEventKind, FolderId,
    FolderSummary, LlmEntryName, MessageSource, Role, Session, SessionId, ThinkingContent,
    TriggerSource, User,
};
use baybo_session::SessionError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::api::dto::{ErrorBody, ListResponse};
use crate::auth::{AuthedClient, WEB_OPERATOR_USER_ID};
use crate::server::AdminState;
use crate::{GatewayError, Result};

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(create_session))
        .routes(routes!(list_sessions))
        .routes(routes!(get_session))
        .routes(routes!(sync_session))
        .routes(routes!(lookup_session_message))
        .routes(routes!(set_session_model))
        .routes(routes!(set_session_pin))
        .routes(routes!(mark_session_read))
        .routes(routes!(set_session_folder))
        .routes(routes!(delete_session))
        .routes(routes!(unhide_session))
        .routes(routes!(slash_manifest))
        .routes(routes!(list_cron_messages))
        .routes(routes!(list_folders))
        .routes(routes!(create_folder))
        .routes(routes!(update_folder))
        .routes(routes!(move_folder))
        .routes(routes!(reorder_folders))
        .routes(routes!(delete_folder))
}

/// Query string for `GET /v1/chat/sessions`.
#[derive(Debug, Default, Deserialize, IntoParams)]
pub struct ListSessionsQuery {
    /// Include hidden sessions in the response. Defaults to false.
    #[serde(default)]
    pub include_hidden: bool,
    /// Include cron-triggered sessions in the response. Defaults to
    /// false so the chat sidebar stays free of background fires; the
    /// dedicated `GET /v1/chat/cron-messages` endpoint surfaces those
    /// in their own pane.
    #[serde(default)]
    pub include_cron: bool,
}

/// Default page size for reverse transcript pagination — large enough
/// that a typical chat fits in one round-trip, small enough that a
/// thousand-turn session doesn't ship in full on the initial GET. Also
/// the default sync page: clients pass this for baseline/cold opens
/// (where `since` is absent and the page is a REPLACE by definition).
pub const DEFAULT_HISTORY_LIMIT: usize = 50;
/// Hard cap so a misbehaving (or curious) client can't ask for the
/// whole transcript by passing `limit=999999`. Shared by history paging
/// and sync — clients pass it explicitly when merging a sync difference
/// into an already-rendered thread (a rebase is a REPLACE, so
/// incremental merge is preferred all the way to the cap).
pub const MAX_HISTORY_LIMIT: usize = 200;
/// How many raw persisted rows a sync difference scan may cover per
/// emitted-row `limit` before giving up and rebasing. The rebase test
/// counts *emitted* transcript rows (an agentic turn persists hundreds
/// of invisible tool rows per handful of visible ones), so the raw scan
/// needs its own bound to keep a pathological gap from an unbounded
/// walk; hitting the bound also rebases.
const SYNC_SCAN_BOUND_MULTIPLIER: usize = 10;

/// Maximum length of the truncated preview the sidebar shows for each
/// session. Sized to fit a 260px-wide sidebar row at the web client's
/// font without wrapping; the client may truncate further with CSS.
const PREVIEW_MAX_CHARS: usize = 120;

/// Ceiling for the chat-list unread badge. A session with more unread replies
/// than this reports exactly this, and the client renders it as "N+". Bounds
/// the per-session count scan (see `SessionManager::unread_reply_count`).
const UNREAD_COUNT_CAP: usize = 99;

/// Default page size for the cron-messages list. Tuned to roughly
/// match what the right-side panel renders before the user scrolls.
const DEFAULT_CRON_MESSAGE_LIMIT: usize = 50;
/// Hard cap on the cron-messages list so a curious client can't ask
/// the gateway to walk thousands of cron sessions in one shot.
const MAX_CRON_MESSAGE_LIMIT: usize = 200;
/// How deep to walk a cron session's transcript when extracting the
/// prompt/response previews. Cron sessions are one-shot — a small
/// constant covers the realistic shape (the framed trigger prompt
/// followed by one or two assistant turns).
const CRON_PREVIEW_SCAN_DEPTH: usize = 12;

/// Query string for `GET /v1/chat/sessions/{session_id}`. Reverse-
/// paginates the active transcript: the response carries the
/// most-recent slice, and the client walks backward by setting
/// `before_ordinal` to the lowest ordinal it has seen so far.
#[derive(Debug, Default, Deserialize, IntoParams)]
pub struct GetSessionQuery {
    /// Return only rows whose `ordinal` is strictly less than this
    /// value. Omit on the initial fetch; pass the lowest ordinal from
    /// the prior page to scroll further back. Maps to a primary-key
    /// range scan over the `session_messages` active index.
    #[serde(default)]
    pub before_ordinal: Option<i64>,
    /// Maximum rows to return. Defaults to
    /// [`DEFAULT_HISTORY_LIMIT`], clamped to [`MAX_HISTORY_LIMIT`].
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Query string for `GET /v1/chat/sessions/{session_id}/sync` — the one
/// forward-recovery pull. `since_ordinal` is the client's cursor; absent
/// means "baseline me on the newest page".
#[derive(Debug, Default, Deserialize, IntoParams)]
pub struct SyncSessionQuery {
    /// Highest coverage watermark the client holds for this session.
    /// Omit for the newest-page baseline (cold start, fresh install,
    /// no local cursor).
    #[serde(default)]
    pub since_ordinal: Option<i64>,
    /// Maximum transcript rows to return, counted in *emitted* rows.
    /// Defaults to [`DEFAULT_HISTORY_LIMIT`], clamped to
    /// [`MAX_HISTORY_LIMIT`]. A difference larger than this rebases.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Query string for `GET /v1/chat/sessions/{session_id}/messages` — the
/// per-send durability point lookup.
#[derive(Debug, Deserialize, IntoParams)]
pub struct MessageLookupQuery {
    /// Client-generated send idempotency key to probe.
    pub platform_msg_id: String,
}

// ── DTOs ─────────────────────────────────────────────────────────────

/// Response from `POST /v1/chat/sessions`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChatSessionCreated {
    /// New session id.
    pub session_id: String,
}

/// Request body for `POST /v1/chat/sessions`.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct CreateSessionRequest {
    /// Optional client-supplied session id. If omitted, the gateway mints one.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, ToSchema)]
struct OptionalCreateSessionRequest(#[schema(inline)] CreateSessionRequest);

impl<S> FromRequest<S> for OptionalCreateSessionRequest
where
    S: Send + Sync,
{
    type Rejection = GatewayError;

    async fn from_request(req: Request, state: &S) -> std::result::Result<Self, Self::Rejection> {
        let body = Bytes::from_request(req, state)
            .await
            .map_err(|e| GatewayError::BadRequest(format!("read create session body: {e}")))?;
        parse_create_session_request(&body).map(Self)
    }
}

/// Discriminator for [`ChatTranscriptItem`] — serialized as
/// `"message"` / `"work"`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptItemKind {
    /// A user or final-assistant bubble.
    Message,
    /// A reconstructed collapsed work block for a tool-using turn.
    Work,
    /// A persisted out-of-band notice (e.g. a `/compact` confirmation) — the
    /// durable shadow of a live `AgentEvent::Notice`. Carries
    /// [`ChatTranscriptItem::text`] and its severity in
    /// [`ChatTranscriptItem::notice_level`]; the client renders it as a colored
    /// notice bar, not a bubble.
    Notice,
}

/// Kind of a reconstructed [`ChatWorkStep`] — serialized as
/// `"reasoning"` / `"prose"` / `"tool"`.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkStepKind {
    Reasoning,
    Prose,
    Tool,
}

/// One transcript row, flattened from `ChatMessage` into a shape the
/// web client can render without re-implementing the content-block
/// matcher. Two shapes ride this struct, discriminated by [`Self::kind`]:
/// a `Message` (user / final-assistant bubble) or a `Work` (reconstructed
/// collapsed work block for a tool-using turn — see
/// [`reconstruct_transcript`]).
#[derive(Debug, Serialize, ToSchema)]
pub struct ChatTranscriptItem {
    /// Stable row id, unique within the session and identical on every
    /// redelivery (sync, backfill): `m<ordinal>` for a message,
    /// `w<ordinal>` for a work block, `n<seq>` for a control-event row.
    /// This is the client's render key AND redelivery dedup key.
    pub id: String,
    /// `session_messages.ordinal` for ordinal-addressed rows: a
    /// `message` carries its own; a `work` item carries the turn's
    /// first intermediate ordinal so it sorts just after the user turn.
    /// **Absent for `notice` / control-echo items** — control events
    /// are not ordinal-addressed (they anchor at an ordinal and are
    /// keyed by their own per-session `seq`, baked into [`Self::id`]).
    /// Clients must not use this for pagination / cursor seeding; see
    /// `ChatSessionDetail::oldest_ordinal` / `newest_ordinal` and
    /// `ChatSyncResponse::next_cursor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinal: Option<i64>,
    /// Message bubble vs. reconstructed work block.
    pub kind: TranscriptItemKind,
    /// `"user"` or `"assistant"` (or `"system"`). String rather than
    /// enum to keep the wire forgiving. Empty for `work` items.
    pub role: String,
    /// Plain text content, newline-joined when multiple text blocks
    /// were present. Empty when the message was media-only or for `work`
    /// items.
    pub text: String,
    /// `true` when this row had non-text content (image / audio /
    /// file). The web client currently shows a placeholder.
    pub has_attachments: bool,
    /// Client-generated send idempotency key for a user `message` row,
    /// when the send carried one. Every redelivery (sync, backfill)
    /// carries it so the client outbox can match durability
    /// confirmations and dedup redelivered rows against the live echo.
    /// Empty for rows without one (assistant replies, work, notices).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub platform_msg_id: String,
    /// Blob attachments for message items. This mirrors the chat WS attachment
    /// shape so native clients can rebuild historical image/file bubbles from
    /// the REST transcript.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<ChatAttachment>,
    /// Wall-clock time the row was persisted, sourced from
    /// `session_messages.created_at`. Lets the client render a
    /// per-message timestamp without a second lookup. Live WS frames
    /// don't carry this — the web client falls back to the receive
    /// time for those, which is close enough for live emissions and
    /// drifts only on catch-up replays (where the row is also
    /// reachable via the REST history surface with the real value).
    pub created_at: DateTime<Utc>,
    /// Reconstructed progress steps for a `work` item (reasoning, tool
    /// calls + results, mid-turn narration). Empty for `message` items.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<ChatWorkStep>,
    /// Open / close wall-clock of a `work` item, derived from the turn's
    /// message timestamps — drives the `Worked Xs` label. `None` for
    /// `message` items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_ended_at: Option<DateTime<Utc>>,
    /// `true` when this `work` item belongs to a turn that was cancelled
    /// (e.g. `/stop`) rather than run to a normal reply — the client labels it
    /// "Cancelled" instead of a plain `Worked Xs`. Always false for
    /// `message` / `notice` items.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
    /// Severity of a `notice` item (`"info"` / `"warn"` / `"error"`), so a reload
    /// colors it the way the live frame did. `None` for `message` / `work` items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice_level: Option<String>,
}

/// A blob attachment embedded in a historical chat transcript item.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ChatAttachment {
    pub kind: String,
    pub blob_id: String,
    pub mime_type: String,
    pub size: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

impl From<WireAttachment> for ChatAttachment {
    fn from(att: WireAttachment) -> Self {
        let kind = match att.kind {
            AttachmentKind::Image => "image",
            AttachmentKind::Audio => "audio",
            AttachmentKind::File => "file",
        };
        Self {
            kind: kind.to_owned(),
            blob_id: att.blob_id,
            mime_type: att.mime_type,
            size: att.size,
            filename: att.filename,
        }
    }
}

/// Response from `GET /v1/chat/sessions/{session_id}/sync` — the one
/// forward-recovery pull. Full-fidelity on every path: rows carry work
/// blocks and notices exactly like the history surface.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChatSyncResponse {
    /// Transcript rows (message | work | notice), ascending. On a
    /// baseline / rebased response this is the newest page and REPLACEs
    /// the client's thread; on a difference response it appends/merges.
    pub rows: Vec<ChatTranscriptItem>,
    /// Coverage watermark: the highest persisted ordinal the scan
    /// covered, visible or not — it may exceed every row in `rows`
    /// (invisible tool/system tail). This, not any row's ordinal, is
    /// what advances the client cursor (`max`-wins). `null` iff the
    /// session has no persisted rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<i64>,
    /// `true` ⇒ `rows` is the NEWEST page, not the requested difference
    /// (the difference exceeded `limit` in emitted rows, or the raw
    /// scan bound). The client REPLACEs its thread with the page and
    /// treats its cursor as rebase-dirty until one non-rebased sync
    /// completes.
    pub rebased: bool,
    /// Page floor for lazy backfill after a REPLACE (baseline /
    /// rebase). Absent on a difference response — the client keeps its
    /// own floor when merging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_ordinal: Option<i64>,
    /// Whether older history exists below `oldest_ordinal` (REPLACE
    /// responses only; always `false` on a difference response).
    pub has_more_older: bool,
}

/// Response from `GET /v1/chat/sessions/{session_id}/messages` — the
/// per-`platform_msg_id` durability probe. `found: false` is a provable
/// absence (the key was never persisted for this session), which lets
/// the client outbox resume its retry machine; `found: true` confirms
/// durability without consuming a retry transmission.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChatMessageLookup {
    pub found: bool,
    /// Ordinal of the newest persisted row carrying the key, when found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinal: Option<i64>,
}

/// One reconstructed step inside a `work` transcript item — the durable
/// shadow of a live turn-progress event, rebuilt from persisted content
/// blocks so a reloaded transcript shows the same collapsed work summary
/// the live view did. `reasoning` / `prose` carry [`Self::text`]; `tool`
/// carries the call's name + a re-derived result summary.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChatWorkStep {
    pub kind: WorkStepKind,
    /// Reasoning trace or mid-turn narration body. Empty for `tool` steps.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// Tool name, set when `kind == Tool`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Short, best-effort label for the call (a path / command / url
    /// pulled from the call input), absent the live `progress_label`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_label: Option<String>,
    /// `"ok"` / `"error"` / `"denied"`, derived from the persisted result
    /// so reload can color-code failures the way the live view did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_status: Option<String>,
    /// One-line summary re-derived from the persisted tool result. `None`
    /// when the result for this call didn't land in the fetched window.
    ///
    /// Deliberately a snippet of the actual result, not a content-light
    /// count: this surface is the bearer-gated, operator-only chat reload
    /// (never the live multi-channel fan-out), so it favors debugging
    /// usefulness. Unlike the live `ToolCompleted.summary` it is NOT run
    /// through `sanitize_stream_fragment`, so it can show raw tool output
    /// the live UI withheld — acceptable for the operator's own view.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_summary: Option<String>,
}

impl ChatWorkStep {
    fn reasoning(text: String) -> Self {
        Self {
            kind: WorkStepKind::Reasoning,
            text,
            tool: None,
            tool_label: None,
            tool_status: None,
            tool_summary: None,
        }
    }

    fn prose(text: String) -> Self {
        Self {
            kind: WorkStepKind::Prose,
            text,
            tool: None,
            tool_label: None,
            tool_status: None,
            tool_summary: None,
        }
    }

    fn tool(tool: String, tool_label: Option<String>) -> Self {
        Self {
            kind: WorkStepKind::Tool,
            text: String::new(),
            tool: Some(tool),
            tool_label,
            tool_status: None,
            tool_summary: None,
        }
    }
}

/// Project the shared wire fold onto the REST shape. The REST surface drops
/// the wire step's `call_id` (only the live client needs it, to pair a later
/// `ToolCompleted`); `status` / `summary` map straight onto `tool_status` /
/// `tool_summary` (both `None` while a tool is still running).
impl From<WireWorkStep> for ChatWorkStep {
    fn from(step: WireWorkStep) -> Self {
        match step.kind {
            WireWorkStepKind::Reasoning => Self::reasoning(step.text),
            WireWorkStepKind::Prose => Self::prose(step.text),
            WireWorkStepKind::Tool => Self {
                kind: WorkStepKind::Tool,
                text: String::new(),
                tool: step.tool,
                tool_label: step.label,
                tool_status: step.status,
                tool_summary: step.summary,
            },
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ChatSessionDetail {
    pub session_id: String,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    pub hidden: bool,
    /// Active transcript slice, oldest-first within the page. Interleaves the
    /// real message rows with out-of-band control events (slash echoes /
    /// notices); a control-event item carries a synthetic negative `ordinal`, so
    /// the client must NOT infer page bounds from transcript items — use
    /// [`Self::oldest_ordinal`] / [`Self::newest_ordinal`] instead.
    pub transcript: Vec<ChatTranscriptItem>,
    /// `true` when at least one older active row exists below this page — i.e.
    /// the client should keep scroll-up pagination armed. `false` when the slice
    /// already includes the session's first message.
    pub has_more: bool,
    /// Lowest / highest real `session_messages.ordinal` in this page (`null` for
    /// an empty page). The client pages older with `before_ordinal = oldest`,
    /// and seeds the WS replay cursor from `newest` — both must ignore the
    /// synthetic ordinals on control-event transcript items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_ordinal: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_ordinal: Option<i64>,
    /// Per-session LLM pin (`session.state.last_llm`): the `baybo.json`
    /// entry name this session's turns resolve against, or `null` to
    /// follow `default-llm`. Drives the chat header model picker's
    /// initial selection. Set via `PUT /v1/chat/sessions/{id}/model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_llm: Option<String>,
    /// Auto-generated conversation title, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ChatSessionSummary {
    pub session_id: String,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    /// True when the user has hidden this session from their chat
    /// list. Only ever populated in responses when
    /// `include_hidden=true` was requested.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
    /// True when the user has pinned this session to the top of their
    /// chat list. Always emitted so the sidebar can place every row in
    /// the right block; set via `PUT /v1/chat/sessions/{id}/pin`.
    pub pinned: bool,
    /// Preview text drawn from the session's most-recent user-authored
    /// message, truncated to [`PREVIEW_MAX_CHARS`]. The web sidebar
    /// renders this as the row label so users can scan past
    /// conversations by what they last asked. `None` for sessions
    /// without a user turn yet (a freshly-created row, or one whose
    /// transcript holds only system/tool rows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_user_text: Option<String>,
    /// The user-created folder this session is filed under, or absent for
    /// uncategorized. Set via `PUT /v1/chat/sessions/{id}/folder`; the web
    /// sidebar groups rows by this id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<String>,
    /// Number of unread assistant replies — final assistant messages persisted
    /// with `ordinal` above this session's read cursor
    /// (`PUT /v1/chat/sessions/{id}/read`), capped at [`UNREAD_COUNT_CAP`]
    /// (the client renders the cap as "N+"). Server-computed, so it is
    /// accurate across a cold restart / a device that missed the live
    /// `SessionActivity` pings. `0` when caught up.
    pub unread_count: i64,
    /// Auto-generated conversation title, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ChatSessionsList {
    pub items: Vec<ChatSessionSummary>,
}

/// One entry in the cron-messages list. Each row corresponds to a
/// distinct cron fire (cron creates a fresh session per trigger, so
/// `session_id` uniquely identifies the fire). The chat surface
/// surfaces these in a right-side notification pane rather than the
/// main sidebar so unattended cron output doesn't bury user-driven
/// conversations.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChatCronMessage {
    /// The session created for this cron fire. Reuse against
    /// `/v1/chat/sessions/{id}` to drill into the full transcript.
    pub session_id: String,
    /// Cron job that produced this fire. Stable across fires of the
    /// same job; missing in the (theoretically impossible) case of a
    /// trigger without a job id.
    pub cron_job_id: String,
    /// When the cron session was created — the actual fire time, to
    /// within scheduler tick precision.
    pub fired_at: DateTime<Utc>,
    /// Latest activity timestamp on the session. Lets the panel sort
    /// by "freshest" without the client having to fetch transcripts.
    pub last_active: DateTime<Utc>,
    /// The user-facing prompt for this fire. Truncated to
    /// [`PREVIEW_MAX_CHARS`]. The persisted user row carries the cron
    /// dispatcher's fire-time framing; `baybo_context::prompts::cron::original_cron_prompt`
    /// recovers the instruction as configured so the panel shows that,
    /// not the framing boilerplate.
    pub prompt: String,
    /// Latest assistant text — what the agent produced in response.
    /// `None` while the fire is still running or if the agent emitted
    /// only tool calls / attachments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ChatCronMessagesList {
    pub items: Vec<ChatCronMessage>,
}

/// Query string for `GET /v1/chat/cron-messages`.
#[derive(Debug, Default, Deserialize, IntoParams)]
pub struct ListCronMessagesQuery {
    /// Maximum number of cron messages to return. Defaults to
    /// [`DEFAULT_CRON_MESSAGE_LIMIT`], clamped to
    /// [`MAX_CRON_MESSAGE_LIMIT`].
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Wire DTO for slash command entries. Mirror of
/// [`baybo_channels::wire::SlashCommandSpec`] so the OpenAPI surface
/// stays inside this crate's DTOs (the wire type lives in
/// `baybo-channels` for sidecar reuse).
#[derive(Debug, Serialize, ToSchema)]
pub struct SlashCommandEntry {
    pub command: String,
    pub description: String,
}

impl From<SlashCommandSpec> for SlashCommandEntry {
    fn from(spec: SlashCommandSpec) -> Self {
        Self {
            command: spec.command,
            description: spec.description,
        }
    }
}

// ── Handlers ─────────────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/chat/sessions",
    tag = "chat",
    request_body = CreateSessionRequest,
    responses(
        (status = 200, description = "New session id", body = ChatSessionCreated),
        (status = 400, description = "Invalid request", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Session creation failed", body = ErrorBody),
    )
)]
async fn create_session(
    State(state): State<AdminState>,
    authed: Option<Extension<AuthedClient>>,
    OptionalCreateSessionRequest(requested): OptionalCreateSessionRequest,
) -> Result<Json<ChatSessionCreated>> {
    let authed = authed.as_ref().map(|ext| &ext.0);
    let user = chat_user(authed);
    let channel_type = chat_list_channel(authed);
    let session =
        create_or_load_chat_session(&state, requested.session_id, user, channel_type.clone())
            .await?;
    let session_id = session.id.clone();
    // Created emits a full patch — sibling tabs construct the row
    // straight from this without a list refetch.
    broadcast_session_patch(
        &state,
        &channel_type,
        &session_id,
        SessionPatch {
            created_at: Some(session.created_at),
            last_active: Some(session.last_active),
            hidden: Some(session.hidden),
            pinned: Some(session.pinned),
            // A freshly-created session is always uncategorized; absent =
            // no change, which a newly-constructed client row renders as
            // uncategorized.
            folder_id: None,
            title: None,
        },
    );
    Ok(Json(ChatSessionCreated {
        session_id: session_id.to_string(),
    }))
}

#[utoipa::path(
    get,
    path = "/chat/sessions",
    tag = "chat",
    params(ListSessionsQuery),
    responses(
        (status = 200, description = "Web chat sessions, newest first", body = ChatSessionsList),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn list_sessions(
    State(state): State<AdminState>,
    Query(query): Query<ListSessionsQuery>,
    authed: Option<Extension<AuthedClient>>,
) -> Result<Json<ChatSessionsList>> {
    let channel_type = chat_list_channel(authed.as_ref().map(|ext| &ext.0));
    // Push the channel filter into SQL so a long-running gateway
    // with thousands of bot sessions (telegram / weixin / …) doesn't
    // pay an O(all-sessions) libsql round-trip on every chat-list
    // refresh — see `SessionStore::list_by_channel`. We still walk
    // the result to apply the hidden filter; that's a userland-only
    // pass over the (now scoped) result.
    //
    // Going through `session_manager` here rather than the trace
    // summary listing is deliberate: fresh chat sessions don't have
    // any trace summary rows yet, so the summary path would hide
    // them until the first agent turn ran.
    let scoped = state
        .session_manager
        .list_by_channel(&channel_type)
        .await
        .map_err(|e| GatewayError::Internal(format!("list sessions: {e}")))?;
    let visible: Vec<Session> = scoped
        .into_iter()
        .filter(|s| query.include_hidden || !s.hidden)
        .filter(|s| query.include_cron || !is_cron_triggered(s))
        .collect();
    // Fan out the per-session preview fetch — each row is a single
    // back-of-the-index lookup (`load_last_user_message`,
    // `ORDER BY ordinal DESC LIMIT 1`) but they add up serially when a tab
    // has dozens of conversations open. `join_all` runs the libsql queries
    // concurrently against the shared connection pool. A preview that
    // fails to load is dropped to `None` rather than failing the whole
    // list — the sidebar still renders the row, just without a
    // preview, and the next list refresh will retry.
    let previews = futures::future::join_all(visible.iter().map(|s| {
        let manager = state.session_manager.clone();
        let sid = s.id.clone();
        async move { last_user_preview(&manager, &sid).await }
    }))
    .await;
    // Fan out the per-session unread count the same way: bounded read-cursor
    // scans, concurrent, each degrading to `0` on error so one bad row can't
    // fail the whole list. Server-computed so it survives a cold restart and
    // is consistent across devices (unlike a client-local ping counter).
    let unread_counts = futures::future::join_all(visible.iter().map(|s| {
        let manager = state.session_manager.clone();
        let sid = s.id.clone();
        async move {
            manager
                .unread_reply_count(&sid, UNREAD_COUNT_CAP)
                .await
                .unwrap_or(0)
        }
    }))
    .await;
    let items: Vec<ChatSessionSummary> = visible
        .into_iter()
        .zip(previews)
        .zip(unread_counts)
        .map(|((s, last_user_text), unread)| ChatSessionSummary {
            session_id: s.id.to_string(),
            created_at: s.created_at,
            last_active: s.last_active,
            hidden: s.hidden,
            pinned: s.pinned,
            last_user_text,
            folder_id: s.folder_id.as_ref().map(|f| f.to_string()),
            unread_count: unread as i64,
            title: s.title.clone(),
        })
        .collect();
    Ok(Json(ChatSessionsList { items }))
}

#[utoipa::path(
    get,
    path = "/chat/sessions/{session_id}",
    tag = "chat",
    params(
        ("session_id" = String, Path, description = "Session id to fetch"),
        GetSessionQuery,
    ),
    responses(
        (status = 200, description = "Session detail + transcript slice", body = ChatSessionDetail),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Session not found", body = ErrorBody),
    )
)]
async fn get_session(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
    Query(query): Query<GetSessionQuery>,
    authed: Option<Extension<AuthedClient>>,
) -> Result<Json<ChatSessionDetail>> {
    let authed = authed.as_ref().map(|ext| &ext.0);
    let (sid, session) = load_scoped_chat_session(&state, &session_id, authed).await?;

    let limit = query
        .limit
        .unwrap_or(DEFAULT_HISTORY_LIMIT)
        .clamp(1, MAX_HISTORY_LIMIT);
    let page = build_history_page(&state, &sid, &session, query.before_ordinal, limit).await?;
    Ok(Json(ChatSessionDetail {
        session_id,
        created_at: session.created_at,
        last_active: session.last_active,
        hidden: session.hidden,
        transcript: page.transcript,
        has_more: page.has_more,
        oldest_ordinal: page.oldest_ordinal,
        newest_ordinal: page.newest_ordinal,
        last_llm: session.state.last_llm.as_ref().map(|n| n.to_string()),
        title: session.title.clone(),
    }))
}

/// One reconstructed transcript page plus its real bounds. Shared by the
/// backward-paging `GET /chat/sessions/{id}` surface and sync's
/// baseline / rebase path (which is the same newest page by definition).
struct HistoryPage {
    transcript: Vec<ChatTranscriptItem>,
    has_more: bool,
    oldest_ordinal: Option<i64>,
    newest_ordinal: Option<i64>,
}

/// Rebuild one full-fidelity transcript page: user/assistant bubbles AND
/// the collapsed per-turn work blocks (reasoning, tool calls + results,
/// mid-turn narration) from the persisted messages, with out-of-band
/// control events interleaved at their anchors. Internal turns
/// (Role::System, agent-injected Role::User with `from_user=false`) are
/// dropped; tool-use iterations are folded into a `work` item rather
/// than surfaced as stray bubbles. Fidelity is a property of the data,
/// not the path that fetched it — every read surface goes through here.
async fn build_history_page(
    state: &AdminState,
    sid: &SessionId,
    session: &Session,
    before_ordinal: Option<i64>,
    limit: usize,
) -> Result<HistoryPage> {
    // Over-fetch by one row so we can answer `has_more` without an
    // extra COUNT — if the store returned `limit + 1` rows, there's at
    // least one older row beyond the window, and we drop the extra
    // before serialising.
    let mut tail = state
        .session_manager
        .history_tail(sid, before_ordinal, limit + 1)
        .await
        .map_err(|e| GatewayError::Internal(format!("load history tail: {e}")))?;
    let has_more = tail.len() > limit;
    if has_more {
        // The overflow row is the *oldest* in the slice — `tail` is in
        // ascending ordinal order, so the unwanted row sits at the head
        // (it would be the start of the next-older page).
        tail.remove(0);
    }
    // Real page bounds (control-event items carry no ordinal, so the
    // client can't infer these from the transcript — it gets them here).
    let oldest_ordinal = tail.first().map(|(o, _, _)| *o);
    let newest_ordinal = tail.last().map(|(o, _, _)| *o);
    let attachment_map = transcript_attachments(&tail, state.blob_store.as_ref()).await;
    // Out-of-band control events (slash-command echoes + notices) live in their
    // own table; interleave those whose `after_ordinal` anchor falls within this
    // page. `upper` is the page's last row; `lower` is its first, except the
    // oldest page (`!has_more`) extends down to catch `-1` / pre-supersession
    // anchors. `reconstruct_transcript` places each event right after its anchor.
    let control_events: Vec<ControlEvent> = match (oldest_ordinal, newest_ordinal) {
        (Some(first), Some(last)) => {
            let lower = if has_more { first } else { i64::MIN };
            match state.session_manager.list_control_events(sid).await {
                Ok(events) => events
                    .into_iter()
                    .filter(|ev| ev.after_ordinal >= lower && ev.after_ordinal <= last)
                    .collect(),
                Err(e) => {
                    tracing::warn!(session_id = %sid, error = %e, "chat: list control events failed");
                    Vec::new()
                }
            }
        }
        _ => Vec::new(),
    };
    // Only the newest page (no `before_ordinal`) can contain the in-flight
    // turn, so only there does aligning the trailing work block's start with
    // the live `TurnState` matter. Best-effort: a lookup miss just leaves the
    // message-timestamp start (the worst case is the pre-existing split).
    let active_turn_started = if before_ordinal.is_none() {
        state
            .job_lifecycle
            .active_turn_started_at(sid)
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    // The in-flight turn's reasoning / tool steps are still streaming and not
    // yet persisted, so a tab loading mid-turn would miss everything thought
    // before it joined. Fold the live channel's per-session buffer of that
    // progress into the trailing work block. Newest page only (where the
    // in-flight turn lives) and only while a turn is active.
    let in_flight_steps = if active_turn_started.is_some() {
        state
            .channel_registry
            .get(&session.channel)
            .map(|ch| in_flight_work_steps(ch.in_flight_events(sid)))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let transcript = reconstruct_transcript_with_attachments(
        tail,
        control_events,
        active_turn_started,
        in_flight_steps,
        &attachment_map,
    );
    Ok(HistoryPage {
        transcript,
        has_more,
        oldest_ordinal,
        newest_ordinal,
    })
}

#[utoipa::path(
    get,
    path = "/chat/sessions/{session_id}/sync",
    tag = "chat",
    params(
        ("session_id" = String, Path, description = "Session id to sync"),
        SyncSessionQuery,
    ),
    responses(
        (status = 200, description = "Forward-recovery pull: the difference after the cursor, or a newest-page baseline (rebased / no cursor)", body = ChatSyncResponse),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Session not found", body = ErrorBody),
    )
)]
async fn sync_session(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
    Query(query): Query<SyncSessionQuery>,
    authed: Option<Extension<AuthedClient>>,
) -> Result<Json<ChatSyncResponse>> {
    let authed = authed.as_ref().map(|ext| &ext.0);
    let (sid, session) = load_scoped_chat_session(&state, &session_id, authed).await?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_HISTORY_LIMIT)
        .clamp(1, MAX_HISTORY_LIMIT);

    let rebased = if let Some(since) = query.since_ordinal {
        match sync_difference(&state, &sid, since, limit).await? {
            Some(response) => return Ok(Json(response)),
            // Difference exceeded the limit (in emitted rows) or the raw
            // scan bound — fall through to a newest-page rebase.
            None => true,
        }
    } else {
        // No cursor: the baseline IS a newest-page REPLACE by
        // definition, so rebase semantics carry no extra meaning.
        false
    };

    let page = build_history_page(&state, &sid, &session, None, limit).await?;
    Ok(Json(ChatSyncResponse {
        rows: page.transcript,
        // The tail scan starts at the newest persisted row, so the
        // page's newest ordinal IS the coverage watermark; `None` iff
        // the session has no rows.
        next_cursor: page.newest_ordinal,
        rebased,
        oldest_ordinal: page.oldest_ordinal,
        has_more_older: page.has_more,
    }))
}

/// Build the difference response for `sync(since)`, or `None` when the
/// difference is too wide and the caller must rebase. The rebase test
/// counts **emitted** transcript rows against `limit` — an agentic turn
/// persists hundreds of invisible tool rows per handful of visible ones,
/// and counting scanned rows would force a mid-stream REPLACE under a
/// watching user — with a raw scan bound as the safety valve.
async fn sync_difference(
    state: &AdminState,
    sid: &SessionId,
    since: i64,
    limit: usize,
) -> Result<Option<ChatSyncResponse>> {
    let scan_bound = limit.saturating_mul(SYNC_SCAN_BOUND_MULTIPLIER);
    let raw = state
        .session_manager
        .history_since(sid, since, scan_bound + 1)
        .await
        .map_err(|e| GatewayError::Internal(format!("load sync difference: {e}")))?;
    if raw.len() > scan_bound {
        return Ok(None);
    }
    // Coverage watermark. The scan ran to the end of the active log, so
    // it covered everything up to the newest persisted row: the last raw
    // ordinal, or — when nothing sits above the cursor — the session's
    // newest ordinal overall (`None` only for a rowless session). This
    // MUST be fixed before the control-event scan below: an event
    // written between the two scans anchors at or above this watermark,
    // so the next `>=` select still covers it; the reverse order would
    // lose such an event permanently.
    let next_cursor = match raw.last() {
        Some((ordinal, _, _)) => Some(*ordinal),
        None => state
            .session_manager
            .latest_session_ordinal(sid)
            .await
            .map_err(|e| GatewayError::Internal(format!("load newest ordinal: {e}")))?,
    };
    // Control events are selected at `>=` the cursor — a row anchored
    // exactly at the cursor is re-delivered on purpose (a notice can be
    // written later with an anchor at an ordinal the client already
    // holds), and the client dedups by the stable `n<seq>` row id.
    let upper = next_cursor.unwrap_or(i64::MIN);
    let control_events: Vec<ControlEvent> = if upper >= since {
        match state.session_manager.list_control_events(sid).await {
            Ok(events) => events
                .into_iter()
                .filter(|ev| ev.after_ordinal >= since && ev.after_ordinal <= upper)
                .collect(),
            Err(e) => {
                tracing::warn!(session_id = %sid, error = %e, "chat: list control events failed");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let attachment_map = transcript_attachments(&raw, state.blob_store.as_ref()).await;
    // Durable rows only — the in-flight turn's live steps are the
    // `SubscribeState` bundle's job, NOT sync's. But we DO align the trailing
    // in-flight turn's reconstructed work block to the active turn's
    // `started_at` (one indexed job read): a mid-turn difference reconstructs
    // that turn's persisted tool rows into a partial `work` block, and without
    // the alignment its `w<ordinal>`/start wouldn't match the live
    // (SubscribeState-opened) block — the client would render TWO blocks for
    // one turn. Aligning the start lets the client reconcile them into one.
    let active_turn_started = state
        .job_lifecycle
        .active_turn_started_at(sid)
        .await
        .ok()
        .flatten();
    let transcript = reconstruct_transcript_with_attachments(
        raw,
        control_events,
        active_turn_started,
        Vec::new(),
        &attachment_map,
    );
    if transcript.len() > limit {
        return Ok(None);
    }
    Ok(Some(ChatSyncResponse {
        rows: transcript,
        next_cursor,
        rebased: false,
        // A difference merges into the client's rendered thread; the
        // client keeps its own backfill floor.
        oldest_ordinal: None,
        has_more_older: false,
    }))
}

#[utoipa::path(
    get,
    path = "/chat/sessions/{session_id}/messages",
    tag = "chat",
    params(
        ("session_id" = String, Path, description = "Session id to probe"),
        MessageLookupQuery,
    ),
    responses(
        (status = 200, description = "Durability point lookup for one send idempotency key", body = ChatMessageLookup),
        (status = 400, description = "Empty platform_msg_id", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Session not found", body = ErrorBody),
    )
)]
async fn lookup_session_message(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
    Query(query): Query<MessageLookupQuery>,
    authed: Option<Extension<AuthedClient>>,
) -> Result<Json<ChatMessageLookup>> {
    let authed = authed.as_ref().map(|ext| &ext.0);
    let (sid, _session) = load_scoped_chat_session(&state, &session_id, authed).await?;
    let key = query.platform_msg_id.trim();
    if key.is_empty() {
        return Err(GatewayError::BadRequest(
            "platform_msg_id must be non-empty".to_string(),
        ));
    }
    let ordinal = state
        .session_manager
        .find_message_ordinal_by_platform_msg_id(&sid, key)
        .await
        .map_err(|e| GatewayError::Internal(format!("platform_msg_id lookup: {e}")))?;
    Ok(Json(ChatMessageLookup {
        found: ordinal.is_some(),
        ordinal,
    }))
}

/// Request body for `PUT /v1/chat/sessions/{session_id}/model`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetSessionModelRequest {
    /// `baybo.json` LLM entry name to pin this session to, or `null`
    /// (absent) to clear the pin and follow `default-llm`. Must match a
    /// configured entry — see `GET /v1/llm/models` → `items[].name`.
    #[serde(default)]
    pub llm: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SetSessionModelResponse {
    /// The pin now in effect: the entry name, or `null` for `default-llm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_llm: Option<String>,
    /// `true` when a live actor was re-pinned in place (applies on the
    /// session's next turn); `false` when only the persisted state was
    /// updated because no actor is currently running (the next user
    /// message spawns one that reads the pin).
    pub applied_to_live_actor: bool,
}

#[utoipa::path(
    put,
    path = "/chat/sessions/{session_id}/model",
    tag = "chat",
    params(
        ("session_id" = String, Path, description = "Session id to re-pin"),
    ),
    request_body = SetSessionModelRequest,
    responses(
        (status = 200, description = "Per-session model pin updated; applies from the session's next turn", body = SetSessionModelResponse),
        (status = 400, description = "Unknown LLM entry name", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Session not found", body = ErrorBody),
    )
)]
async fn set_session_model(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
    authed: Option<Extension<AuthedClient>>,
    Json(req): Json<SetSessionModelRequest>,
) -> Result<Json<SetSessionModelResponse>> {
    let authed = authed.as_ref().map(|ext| &ext.0);
    // We only need the existence/scope check, not the loaded blob:
    // persistence goes through the targeted `set_last_llm` below.
    let (sid, _) = load_scoped_chat_session(&state, &session_id, authed).await?;

    let pin: Option<LlmEntryName> = super::validate_llm_pin(&state, req.llm.as_deref())?;

    // Persist the pin durably FIRST, via a targeted flat-column write
    // (`set_last_llm`). Unlike a full-session `save`, this can't be
    // clobbered by a concurrent `touch` (load + full blob save fired on
    // every inbound message) — the same flat-column discipline the
    // `hidden` flag uses. It is synchronous, so a storage failure
    // surfaces as an error here instead of a false 200, and it is
    // authoritative for any actor spawned later (the spawner reads
    // `session.state.last_llm`, which `get` patches from this column).
    state
        .session_manager
        .set_last_llm(&sid, pin.as_ref())
        .await
        .map_err(|e| GatewayError::Internal(format!("persist session model pin: {e}")))?;

    // Then re-pin any *live* actor in memory so the switch takes effect
    // on its next turn without waiting for eviction + rehydration.
    // Unconditional: a `false` return just means no actor is live right
    // now, in which case the persisted pin above already covers the next
    // spawn. (A spawn racing in the µs window between this persist and
    // the route can still start on the prior pin for its lifetime; the
    // store stays correct, so it self-heals on the next eviction.)
    let applied_to_live_actor = state
        .supervisor
        .route(&sid, AgentMessage::SetModel { llm: pin.clone() })
        .await;

    Ok(Json(SetSessionModelResponse {
        last_llm: pin.map(|n| n.to_string()),
        applied_to_live_actor,
    }))
}

/// Request body for `PUT /v1/chat/sessions/{session_id}/pin`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetSessionPinRequest {
    /// `true` to pin this session to the top of the chat list, `false`
    /// to unpin it back into the regular list.
    pub pinned: bool,
}

#[utoipa::path(
    put,
    path = "/chat/sessions/{session_id}/pin",
    tag = "chat",
    params(
        ("session_id" = String, Path, description = "Session id to pin or unpin"),
    ),
    request_body = SetSessionPinRequest,
    responses(
        (status = 204, description = "Pin state updated"),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Session not found", body = ErrorBody),
    )
)]
async fn set_session_pin(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
    authed: Option<Extension<AuthedClient>>,
    Json(req): Json<SetSessionPinRequest>,
) -> Result<axum::http::StatusCode> {
    let authed = authed.as_ref().map(|ext| &ext.0);
    let (sid, session) = load_scoped_chat_session(&state, &session_id, authed).await?;
    // Targeted flat-column write — like `set_hidden`, it survives a
    // concurrent `touch` (full-blob save) so the pin can't be clobbered.
    state
        .session_manager
        .set_pinned(&sid, req.pinned)
        .await
        .map_err(|e| GatewayError::Internal(format!("set session pin: {e}")))?;
    // Broadcast so every open chat tab moves the row to the right block
    // without a list refetch.
    broadcast_session_patch(
        &state,
        &session.channel,
        &sid,
        SessionPatch {
            pinned: Some(req.pinned),
            ..SessionPatch::default()
        },
    );
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Request body for `PUT /v1/chat/sessions/{session_id}/read`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct MarkReadRequest {
    /// Highest `session_messages.ordinal` the viewer has now read. The read
    /// cursor advances max-wins, so a stale/lower value is a no-op — a client
    /// can safely fire this on open and after each new reply while foreground.
    pub ordinal: i64,
}

#[utoipa::path(
    put,
    path = "/chat/sessions/{session_id}/read",
    tag = "chat",
    params(
        ("session_id" = String, Path, description = "Session id to mark read"),
    ),
    request_body = MarkReadRequest,
    responses(
        (status = 204, description = "Read cursor advanced; unread_count recomputes from it"),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Session not found", body = ErrorBody),
    )
)]
async fn mark_session_read(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
    authed: Option<Extension<AuthedClient>>,
    Json(req): Json<MarkReadRequest>,
) -> Result<axum::http::StatusCode> {
    let authed = authed.as_ref().map(|ext| &ext.0);
    let (sid, _) = load_scoped_chat_session(&state, &session_id, authed).await?;
    // Targeted, max-wins flat-column write — survives a concurrent `touch`
    // and can't regress on a reordered request. The list's `unread_count`
    // derives from this on the next fetch; no live broadcast (unread is a
    // per-viewer concern that converges on the next list pull).
    state
        .session_manager
        .set_read_cursor(&sid, req.ordinal)
        .await
        .map_err(|e| GatewayError::Internal(format!("mark session read: {e}")))?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete,
    path = "/chat/sessions/{session_id}",
    tag = "chat",
    params(
        ("session_id" = String, Path, description = "Session id to hide"),
    ),
    responses(
        (status = 204, description = "Hidden (row preserved on the server)"),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Session not found", body = ErrorBody),
    )
)]
async fn delete_session(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
    authed: Option<Extension<AuthedClient>>,
) -> Result<axum::http::StatusCode> {
    // Despite the `DELETE` verb this hides rather than removes the
    // row — see the module docstring. Other tabs keep working because
    // web chat authenticates with the admin bearer, not a session-bound
    // credential; users can restore via `POST .../unhide` or
    // `?include_hidden=true`.
    let authed = authed.as_ref().map(|ext| &ext.0);
    let (sid, session) = load_scoped_chat_session(&state, &session_id, authed).await?;
    state
        .session_manager
        .set_hidden(&sid, true)
        .await
        .map_err(|e| GatewayError::Internal(format!("hide session: {e}")))?;
    broadcast_session_patch(
        &state,
        &session.channel,
        &sid,
        SessionPatch {
            hidden: Some(true),
            ..SessionPatch::default()
        },
    );
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/chat/sessions/{session_id}/unhide",
    tag = "chat",
    params(
        ("session_id" = String, Path, description = "Session id to restore"),
    ),
    responses(
        (status = 204, description = "Unhidden"),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Session not found", body = ErrorBody),
    )
)]
async fn unhide_session(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
    authed: Option<Extension<AuthedClient>>,
) -> Result<axum::http::StatusCode> {
    let authed = authed.as_ref().map(|ext| &ext.0);
    let (sid, session) = load_scoped_chat_session(&state, &session_id, authed).await?;
    state
        .session_manager
        .set_hidden(&sid, false)
        .await
        .map_err(|e| GatewayError::Internal(format!("unhide session: {e}")))?;
    // Full patch — a sibling tab that hid this session won't have it
    // in its current list anymore, and the patch carries enough to
    // re-add the row directly without a list refetch.
    broadcast_session_patch(
        &state,
        &session.channel,
        &sid,
        SessionPatch {
            created_at: Some(session.created_at),
            last_active: Some(session.last_active),
            hidden: Some(false),
            // Carry the live pin state so a sibling tab re-adding the
            // row drops it straight into the correct block.
            pinned: Some(session.pinned),
            // Carry the folder assignment too so the re-added row lands in
            // the right folder (absent ⇒ uncategorized).
            folder_id: session.folder_id.as_ref().map(|f| FolderChange::Set {
                id: f.as_str().to_owned(),
            }),
            title: session.title.clone(),
        },
    );
    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ── Folders ──────────────────────────────────────────────────────────

/// Map a folder-op [`SessionError`] onto the right HTTP status: invariant
/// violations (depth, cycle, name length) are 400; a missing folder /
/// session is 404; anything else is 500.
fn folder_err(e: SessionError) -> GatewayError {
    match e {
        SessionError::NotFound(m) => GatewayError::NotFound(m),
        SessionError::InvalidFolderOp(m) => GatewayError::BadRequest(m),
        other => GatewayError::Internal(format!("folder op: {other}")),
    }
}

/// Broadcast the current folder tree as a full snapshot to every open web or
/// device chat client. Called after any folder mutation. No-op when no subscribed
/// chat channel is installed (test fixtures with no live clients).
async fn broadcast_folders(state: &AdminState) -> Result<()> {
    let folders: Vec<FolderView> = state
        .session_manager
        .list_folders()
        .await
        .map_err(folder_err)?
        .into_iter()
        .map(|f| FolderView {
            id: f.id.to_string(),
            parent_id: f.parent_id.as_ref().map(|p| p.to_string()),
            name: f.name,
            position: f.position,
            created_at: f.created_at,
        })
        .collect();
    for channel_type in chat_broadcast_channels() {
        let Some(channel) = state.channel_registry.get(&channel_type) else {
            continue;
        };
        if let Some(sub) = channel.as_subscribed() {
            sub.broadcast_folders_changed(folders.clone());
        }
    }
    Ok(())
}

/// Request body for `PUT /v1/chat/sessions/{session_id}/folder`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetSessionFolderRequest {
    /// Target folder id, or `null` to clear the assignment (uncategorized).
    #[serde(default)]
    pub folder_id: Option<String>,
}

#[utoipa::path(
    put,
    path = "/chat/sessions/{session_id}/folder",
    tag = "chat",
    params(
        ("session_id" = String, Path, description = "Session id to file"),
    ),
    request_body = SetSessionFolderRequest,
    responses(
        (status = 204, description = "Folder assignment updated"),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Session or folder not found", body = ErrorBody),
    )
)]
async fn set_session_folder(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
    authed: Option<Extension<AuthedClient>>,
    Json(req): Json<SetSessionFolderRequest>,
) -> Result<axum::http::StatusCode> {
    let authed = authed.as_ref().map(|ext| &ext.0);
    let (sid, session) = load_scoped_chat_session(&state, &session_id, authed).await?;
    let folder = req.folder_id.map(FolderId::from);
    state
        .session_manager
        .set_folder(&sid, folder.as_ref())
        .await
        .map_err(folder_err)?;
    let change = match &folder {
        Some(f) => FolderChange::Set {
            id: f.as_str().to_owned(),
        },
        None => FolderChange::Uncategorized,
    };
    broadcast_session_patch(
        &state,
        &session.channel,
        &sid,
        SessionPatch {
            folder_id: Some(change),
            ..SessionPatch::default()
        },
    );
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// One folder in a folder-list / create response.
#[derive(Debug, Serialize, ToSchema)]
pub struct FolderDto {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub name: String,
    pub position: i64,
    pub created_at: DateTime<Utc>,
}

impl From<FolderSummary> for FolderDto {
    fn from(f: FolderSummary) -> Self {
        Self {
            id: f.id.to_string(),
            parent_id: f.parent_id.as_ref().map(|p| p.to_string()),
            name: f.name,
            position: f.position,
            created_at: f.created_at,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct FolderList {
    pub items: Vec<FolderDto>,
}

#[utoipa::path(
    get,
    path = "/chat/folders",
    tag = "chat",
    responses(
        (status = 200, description = "The chat-list folder tree", body = FolderList),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn list_folders(State(state): State<AdminState>) -> Result<Json<FolderList>> {
    let items = state
        .session_manager
        .list_folders()
        .await
        .map_err(folder_err)?
        .into_iter()
        .map(FolderDto::from)
        .collect();
    Ok(Json(FolderList { items }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateFolderRequest {
    pub name: String,
    /// Parent folder id (`null`/absent = top-level).
    #[serde(default)]
    pub parent_id: Option<String>,
}

#[utoipa::path(
    post,
    path = "/chat/folders",
    tag = "chat",
    request_body = CreateFolderRequest,
    responses(
        (status = 200, description = "The created folder", body = FolderDto),
        (status = 400, description = "Invalid name / depth", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Parent folder not found", body = ErrorBody),
    )
)]
async fn create_folder(
    State(state): State<AdminState>,
    Json(req): Json<CreateFolderRequest>,
) -> Result<Json<FolderDto>> {
    let parent = req.parent_id.map(FolderId::from);
    let folder = state
        .session_manager
        .create_folder(parent, req.name)
        .await
        .map_err(folder_err)?;
    broadcast_folders(&state).await?;
    Ok(Json(FolderDto::from(folder)))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateFolderRequest {
    /// New name (absent = unchanged).
    #[serde(default)]
    pub name: Option<String>,
}

#[utoipa::path(
    patch,
    path = "/chat/folders/{folder_id}",
    tag = "chat",
    params(
        ("folder_id" = String, Path, description = "Folder id to rename"),
    ),
    request_body = UpdateFolderRequest,
    responses(
        (status = 204, description = "Folder updated"),
        (status = 400, description = "Invalid name", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Folder not found", body = ErrorBody),
    )
)]
async fn update_folder(
    State(state): State<AdminState>,
    Path(folder_id): Path<String>,
    Json(req): Json<UpdateFolderRequest>,
) -> Result<axum::http::StatusCode> {
    let fid = FolderId::from(folder_id);
    if let Some(name) = req.name {
        state
            .session_manager
            .rename_folder(&fid, name)
            .await
            .map_err(folder_err)?;
    }
    broadcast_folders(&state).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct MoveFolderRequest {
    /// New parent id, or `null` to promote the folder to top-level.
    #[serde(default)]
    pub parent_id: Option<String>,
}

#[utoipa::path(
    post,
    path = "/chat/folders/{folder_id}/move",
    tag = "chat",
    params(
        ("folder_id" = String, Path, description = "Folder id to reparent"),
    ),
    request_body = MoveFolderRequest,
    responses(
        (status = 204, description = "Folder moved"),
        (status = 400, description = "Cycle / depth violation", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Folder not found", body = ErrorBody),
    )
)]
async fn move_folder(
    State(state): State<AdminState>,
    Path(folder_id): Path<String>,
    Json(req): Json<MoveFolderRequest>,
) -> Result<axum::http::StatusCode> {
    let fid = FolderId::from(folder_id);
    let parent = req.parent_id.map(FolderId::from);
    state
        .session_manager
        .reparent_folder(&fid, parent)
        .await
        .map_err(folder_err)?;
    broadcast_folders(&state).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ReorderFoldersRequest {
    /// Parent of the sibling group being reordered (`null` = top-level).
    #[serde(default)]
    pub parent_id: Option<String>,
    /// The sibling ids in their new order.
    pub ordered_ids: Vec<String>,
}

#[utoipa::path(
    post,
    path = "/chat/folders/reorder",
    tag = "chat",
    request_body = ReorderFoldersRequest,
    responses(
        (status = 204, description = "Sibling group reordered"),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn reorder_folders(
    State(state): State<AdminState>,
    Json(req): Json<ReorderFoldersRequest>,
) -> Result<axum::http::StatusCode> {
    let parent = req.parent_id.map(FolderId::from);
    let ids = req.ordered_ids.into_iter().map(FolderId::from).collect();
    state
        .session_manager
        .reorder_folders(parent, ids)
        .await
        .map_err(folder_err)?;
    broadcast_folders(&state).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete,
    path = "/chat/folders/{folder_id}",
    tag = "chat",
    params(
        ("folder_id" = String, Path, description = "Folder id to delete"),
    ),
    responses(
        (status = 204, description = "Folder dissolved (sessions preserved)"),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Folder not found", body = ErrorBody),
    )
)]
async fn delete_folder(
    State(state): State<AdminState>,
    Path(folder_id): Path<String>,
) -> Result<axum::http::StatusCode> {
    let fid = FolderId::from(folder_id);
    let affected = state
        .session_manager
        .delete_folder(&fid)
        .await
        .map_err(folder_err)?;
    // Folder structure converges via the snapshot; the chats that fell
    // back to uncategorized converge via a per-session patch each, so a
    // sibling tab moves them live without a list refetch. The patch is
    // channel-scoped, so look up each affected session's home channel;
    // a row that fails to load just skips its patch (the next list
    // refetch converges it).
    broadcast_folders(&state).await?;
    for sid in affected {
        let Ok(Some(session)) = state.session_manager.get(&sid).await else {
            continue;
        };
        broadcast_session_patch(
            &state,
            &session.channel,
            &sid,
            SessionPatch {
                folder_id: Some(FolderChange::Uncategorized),
                ..SessionPatch::default()
            },
        );
    }
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/chat/cron-messages",
    tag = "chat",
    params(ListCronMessagesQuery),
    responses(
        (status = 200, description = "Cron-triggered http sessions with prompt + agent response previews, newest fire first", body = ChatCronMessagesList),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn list_cron_messages(
    State(state): State<AdminState>,
    Query(query): Query<ListCronMessagesQuery>,
) -> Result<Json<ChatCronMessagesList>> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_CRON_MESSAGE_LIMIT)
        .clamp(1, MAX_CRON_MESSAGE_LIMIT);
    let scoped = state
        .session_manager
        .list_by_channel(&ChannelType::http())
        .await
        .map_err(|e| GatewayError::Internal(format!("list cron messages: {e}")))?;
    // `list_by_channel` already returns rows ordered by `last_active`
    // desc; the filter preserves order so the take(limit) below is the
    // freshest N cron fires.
    let cron_sessions: Vec<Session> = scoped
        .into_iter()
        .filter(|s| !s.hidden)
        .filter_map(|s| match &s.trigger {
            TriggerSource::Cron { cron_job_id } if !cron_job_id.is_empty() => Some(s),
            _ => None,
        })
        .take(limit)
        .collect();
    let manager = state.session_manager.clone();
    let items = futures::future::join_all(cron_sessions.into_iter().map(|s| {
        let manager = manager.clone();
        async move { cron_message_from_session(&manager, s).await }
    }))
    .await
    .into_iter()
    .flatten()
    .collect();
    Ok(Json(ChatCronMessagesList { items }))
}

#[utoipa::path(
    get,
    path = "/chat/slash-manifest",
    tag = "chat",
    responses(
        (status = 200, description = "Slash command list for /-autocomplete", body = ListResponse<SlashCommandEntry>),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn slash_manifest(
    State(_state): State<AdminState>,
) -> Result<Json<ListResponse<SlashCommandEntry>>> {
    let items = crate::channel::slash::manifest()
        .into_iter()
        .filter(|c| !WEB_HIDDEN_SLASH_COMMANDS.contains(&c.command.as_str()))
        .map(SlashCommandEntry::from)
        .collect();
    Ok(Json(ListResponse::new(items)))
}

/// Slash commands the gateway dispatcher owns for sidecars but the web
/// composer should NOT advertise. `/new` is a sidecar affordance for
/// resetting a session over a chat surface that has no UI — the web
/// already exposes a "New chat" button, so listing it in the slash
/// palette is just clutter.
const WEB_HIDDEN_SLASH_COMMANDS: &[&str] = &["new"];

// ── helpers ──────────────────────────────────────────────────────────

fn parse_create_session_request(body: &Bytes) -> Result<CreateSessionRequest> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(CreateSessionRequest::default());
    }
    let mut request: CreateSessionRequest = serde_json::from_slice(body)
        .map_err(|e| GatewayError::BadRequest(format!("invalid create session body: {e}")))?;
    if let Some(session_id) = request.session_id.as_mut() {
        *session_id = session_id.trim().to_owned();
        if session_id.is_empty() {
            return Err(GatewayError::BadRequest(
                "session_id must not be empty".to_owned(),
            ));
        }
    }
    Ok(request)
}

async fn create_or_load_chat_session(
    state: &AdminState,
    requested_session_id: Option<String>,
    user: User,
    channel_type: ChannelType,
) -> Result<Session> {
    let Some(session_id) = requested_session_id else {
        return state
            .session_manager
            .create_session(user, channel_type)
            .await
            .map_err(|e| GatewayError::Internal(format!("create chat session: {e}")));
    };

    let sid = SessionId::from(session_id.as_str());
    if let Some(existing) = state
        .session_manager
        .get(&sid)
        .await
        .map_err(|e| GatewayError::Internal(format!("load requested chat session: {e}")))?
    {
        if existing.channel != channel_type
            || existing.user.id != user.id
            || is_cron_triggered(&existing)
        {
            return Err(GatewayError::NotFound(format!("chat session {session_id}")));
        }
        return Ok(existing);
    }

    state
        .session_manager
        .get_or_create(&sid, user, channel_type)
        .await
        .map_err(|e| GatewayError::Internal(format!("create requested chat session: {e}")))
}

fn chat_list_channel(authed: Option<&AuthedClient>) -> ChannelType {
    match authed {
        Some(AuthedClient::Device { .. }) => ChannelType::device(),
        _ => ChannelType::http(),
    }
}

fn chat_user(authed: Option<&AuthedClient>) -> User {
    match authed {
        Some(AuthedClient::Device { device_id }) => User {
            id: device_id.clone(),
            name: None,
            channel: ChannelType::device(),
        },
        _ => web_operator_user(),
    }
}

fn web_operator_user() -> User {
    User {
        id: WEB_OPERATOR_USER_ID.to_owned(),
        name: Some("Web Operator".to_owned()),
        channel: ChannelType::http(),
    }
}

/// Load the session row for `session_id` and verify it lives on the request
/// identity's chat channel (`http` for web, `device` for direct device/relay).
/// Both branches return the **same** `NotFound` body so a request for a
/// Telegram/WeChat session id through the chat API
/// can't be distinguished from a request for a nonexistent id —
/// `GatewayError::NotFound` serialises its `to_string()` into the JSON
/// response, so differing messages would otherwise leak existence.
async fn load_scoped_chat_session(
    state: &AdminState,
    session_id: &str,
    authed: Option<&AuthedClient>,
) -> Result<(SessionId, Session)> {
    let sid = SessionId::from(session_id);
    let not_found = || GatewayError::NotFound(format!("chat session {session_id}"));
    let session = state
        .session_manager
        .get(&sid)
        .await
        .map_err(|e| GatewayError::Internal(format!("load session: {e}")))?
        .ok_or_else(not_found)?;
    if session.channel != chat_list_channel(authed) {
        return Err(not_found());
    }
    Ok((sid, session))
}

/// Push a [`Frame::SessionUpdated`] patch to every open chat client on the
/// session's own channel. Scoped to `channel` on purpose — the `http` (web)
/// and `device` (iOS) channels own disjoint session universes, and fanning a
/// patch across both plants clickable ghost rows for device sessions in web
/// sidebars. The patch carries the truth (no refetch round-trip); see the
/// variant's doc comment for receiver-side merge rules. No-op when the
/// channel is not installed (only possible in test fixtures that skipped
/// `install_channels`).
pub(crate) fn broadcast_session_patch(
    state: &AdminState,
    channel: &ChannelType,
    session_id: &SessionId,
    patch: SessionPatch,
) {
    let Some(channel) = state.channel_registry.get(channel) else {
        return;
    };
    if let Some(sub) = channel.as_subscribed() {
        sub.broadcast_session_patch(session_id.clone(), patch);
    }
}

/// The subscribed chat channels a folder-tree snapshot fans out to.
/// Folders are not session-scoped, so — unlike [`broadcast_session_patch`]
/// — both chat universes receive the (id-only, harmless) snapshot.
fn chat_broadcast_channels() -> [ChannelType; 2] {
    [ChannelType::http(), ChannelType::device()]
}

/// True when the session was spawned by a cron trigger rather than a user
/// conversation. Cron sessions are filtered out of the chat list unless
/// `include_cron` is set.
fn is_cron_triggered(session: &Session) -> bool {
    matches!(session.trigger, TriggerSource::Cron { .. })
}

/// Walk a cron session's tail to extract the prompt + freshest
/// assistant response. Returns `None` only when the walk itself fails;
/// a fire that's still running (no assistant turn yet) returns the
/// prompt with `response = None`, and a fire with no decodable rows
/// at all surfaces empty strings so the panel still pins the
/// timestamp instead of silently dropping the row.
async fn cron_message_from_session(
    manager: &baybo_session::SessionManager,
    session: Session,
) -> Option<ChatCronMessage> {
    let cron_job_id = match &session.trigger {
        TriggerSource::Cron { cron_job_id } => cron_job_id.clone(),
        _ => return None,
    };
    let tail = manager
        .history_tail(&session.id, None, CRON_PREVIEW_SCAN_DEPTH)
        .await
        .ok()?;
    let mut prompt: Option<String> = None;
    let mut response: Option<String> = None;
    // `history_tail` returns ascending; the cron prompt is the first
    // user row (oldest) and the assistant response is the freshest
    // assistant row (newest). Walk forward for the prompt; walk in
    // reverse for the response so we land on the last assistant turn
    // even when the agent emitted multiple.
    for (_ord, _at, msg) in &tail {
        // The cron prompt persists as a `MessageSource::Cron` row; locate it by
        // provenance, then strip the framing for display. It rides as a
        // `Role::User` turn, indistinguishable by role from a skill reminder.
        if matches!(msg.source(), MessageSource::Cron) {
            let text = extract_text(&msg.content);
            prompt = Some(baybo_context::prompts::cron::original_cron_prompt(&text).to_owned());
            break;
        }
    }
    for (_ord, _at, msg) in tail.iter().rev() {
        if matches!(msg.role, Role::Assistant) {
            let text = extract_text(&msg.content);
            if !text.is_empty() {
                response = Some(truncate_preview(&text));
                break;
            }
        }
    }
    Some(ChatCronMessage {
        session_id: session.id.to_string(),
        cron_job_id,
        fired_at: session.created_at,
        last_active: session.last_active,
        prompt: prompt.map(|p| truncate_preview(&p)).unwrap_or_default(),
        response,
    })
}

fn extract_text(content: &[ContentBlock]) -> String {
    let mut text = String::new();
    for block in content {
        if let ContentBlock::Text(t) = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(t);
        }
    }
    text
}

async fn transcript_attachments(
    rows: &[(i64, DateTime<Utc>, ChatMessage)],
    blob_store: &dyn baybo_store::BlobStore,
) -> HashMap<i64, Vec<ChatAttachment>> {
    let mut attachments = HashMap::new();
    for (ordinal, _created_at, msg) in rows {
        if !msg.content.iter().any(|block| {
            matches!(
                block,
                ContentBlock::Image { .. } | ContentBlock::Audio { .. } | ContentBlock::File { .. }
            )
        }) {
            continue;
        }
        let (_text, wire_attachments) =
            crate::channel::adapter::split_content(&msg.content, blob_store).await;
        if !wire_attachments.is_empty() {
            attachments.insert(
                *ordinal,
                wire_attachments
                    .into_iter()
                    .map(ChatAttachment::from)
                    .collect(),
            );
        }
    }
    attachments
}

/// Fetch the most-recent user-authored text for `session_id` and shape it
/// into the sidebar preview the list endpoint serves. Returns `None` when
/// the session has no user turn, when that turn is media-only, or when the
/// lookup fails — the sidebar treats all three as "no preview" rather than
/// surfacing an error, so a single bad row never breaks the whole list.
/// One indexed lookup ([`baybo_session::SessionManager::last_user_message`]),
/// so a prompt buried under a long tool loop is still found.
async fn last_user_preview(
    manager: &baybo_session::SessionManager,
    session_id: &SessionId,
) -> Option<String> {
    // One indexed lookup for the freshest human-authored turn — no
    // tail-walking, so a long tool loop can't bury the prompt past a fixed
    // window (which used to make a tool-retry session show "New
    // conversation" despite having a clear prompt). `message_item`
    // extracts the display text the same way the transcript does; the
    // ordinal it stamps is unused here.
    let (created_at, msg) = manager.last_user_message(session_id).await.ok().flatten()?;
    let item = message_item(0, created_at, "user", &msg, Vec::new())?;
    (!item.text.is_empty()).then(|| truncate_preview(&item.text))
}

/// Collapse whitespace and clip to [`PREVIEW_MAX_CHARS`] for the
/// sidebar preview. Newlines and runs of spaces would render as a
/// single line in the sidebar's truncating row anyway, so we collapse
/// them here rather than ship the raw multi-line text just to have
/// the client throw the structure away.
fn truncate_preview(text: &str) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= PREVIEW_MAX_CHARS {
        return collapsed;
    }
    let cut: String = collapsed.chars().take(PREVIEW_MAX_CHARS).collect();
    format!("{cut}…")
}

/// Accumulator for one tool-using turn's reconstructed work block, drained
/// by [`Self::flush`] just before the turn's final answer.
#[derive(Default)]
struct WorkAccumulator {
    steps: Vec<ChatWorkStep>,
    /// Ordinal of the turn's first intermediate message — the `work`
    /// item inherits it so it sorts right after the user turn.
    ordinal: Option<i64>,
    started: Option<DateTime<Utc>>,
    last: Option<DateTime<Utc>>,
    /// Set when the turn this block belongs to was cancelled (`/stop`) — the
    /// flushed item carries it so the client labels the block "Cancelled".
    cancelled: bool,
    /// `tool_use_id` → index into `steps`, so a later tool-result message
    /// fills in the matching call's summary.
    pending_tools: HashMap<String, usize>,
}

impl WorkAccumulator {
    fn flush(&mut self, items: &mut Vec<ChatTranscriptItem>, ended_at: Option<DateTime<Utc>>) {
        if !self.steps.is_empty() {
            let started = self.started;
            let ordinal = self.ordinal.unwrap_or_default();
            items.push(ChatTranscriptItem {
                id: format!("w{ordinal}"),
                ordinal: Some(ordinal),
                kind: TranscriptItemKind::Work,
                role: String::new(),
                text: String::new(),
                has_attachments: false,
                platform_msg_id: String::new(),
                attachments: Vec::new(),
                created_at: started.unwrap_or_else(Utc::now),
                steps: std::mem::take(&mut self.steps),
                work_started_at: started,
                work_ended_at: ended_at.or(self.last).or(started),
                cancelled: self.cancelled,
                notice_level: None,
            });
        }
        self.ordinal = None;
        self.started = None;
        self.last = None;
        self.cancelled = false;
        self.pending_tools.clear();
    }
}

/// A control event that marks the turn it interrupts as *cancelled* — the
/// `/stop` echo (and only that). `/compact` and other commands sit on turn
/// boundaries but don't cancel the work, so they must not flip the label.
fn is_stop_control_event(ev: &ControlEvent) -> bool {
    ev.kind == ControlEventKind::Command
        && ev
            .text
            .trim()
            .strip_prefix('/')
            .map(|rest| {
                // `/stop`, `/stop@bot`, `/stop arg` → first token is the command.
                let cmd = rest.split([' ', '@']).next().unwrap_or("");
                cmd.eq_ignore_ascii_case(STOP_COMMAND_NAME)
            })
            .unwrap_or(false)
}

/// Rebuild the chat transcript from persisted messages. User turns and the
/// final, tool-call-free assistant reply become `message` items; each
/// tool-using turn's intermediate iterations (reasoning, tool calls +
/// results, mid-turn narration) are folded into a single collapsed `work`
/// item placed just before that turn's final answer. Internal rows
/// (Role::System, agent-injected Role::User) are dropped. This restores on
/// reload the same work-block-then-answer shape the live view shows —
/// turn-progress is otherwise live-only, but every block it needs is
/// durably persisted, so this is where it's reconstructed.
/// Convert a session's buffered in-flight progress (from the live channel,
/// streamed but not yet persisted) into work steps — mirroring how
/// `reconstruct_transcript` derives steps from persisted messages — so a tab
/// loading mid-turn shows the reasoning / tool steps that streamed before it
/// joined. Consecutive reasoning / answer deltas were already coalesced by the
/// channel buffer, so each arrives as a single entry.
fn in_flight_work_steps(events: Vec<SessionEvent>) -> Vec<ChatWorkStep> {
    crate::channel::work_steps::in_flight_wire_steps(events)
        .into_iter()
        .map(ChatWorkStep::from)
        .collect()
}

#[cfg(test)]
fn reconstruct_transcript(
    tail: Vec<(i64, DateTime<Utc>, ChatMessage)>,
    control_events: Vec<ControlEvent>,
    active_turn_started: Option<DateTime<Utc>>,
    in_flight_steps: Vec<ChatWorkStep>,
) -> Vec<ChatTranscriptItem> {
    let attachments_by_ordinal = HashMap::new();
    reconstruct_transcript_with_attachments(
        tail,
        control_events,
        active_turn_started,
        in_flight_steps,
        &attachments_by_ordinal,
    )
}

fn reconstruct_transcript_with_attachments(
    tail: Vec<(i64, DateTime<Utc>, ChatMessage)>,
    control_events: Vec<ControlEvent>,
    active_turn_started: Option<DateTime<Utc>>,
    in_flight_steps: Vec<ChatWorkStep>,
    attachments_by_ordinal: &HashMap<i64, Vec<ChatAttachment>>,
) -> Vec<ChatTranscriptItem> {
    // Merge message rows and out-of-band control events into one ordinal-ordered
    // stream: a control event with `after_ordinal = N` sorts right after the row
    // at ordinal N (and before N+1), `seq`-ordered among events sharing an
    // anchor. Folding then runs over the stream unchanged; a control event just
    // flushes the open work block and emits its own item.
    enum Entry {
        Row(i64, DateTime<Utc>, ChatMessage),
        Control(ControlEvent),
    }
    // Newest persisted ordinal — the in-flight work block (which has no
    // persisted intermediate row of its own to borrow one from) inherits this
    // so its React key is unique and it sorts after the turn's user message.
    let last_ordinal = tail.iter().map(|(o, _, _)| *o).max().unwrap_or(0);
    let mut entries: Vec<(i64, u8, i64, Entry)> =
        Vec::with_capacity(tail.len() + control_events.len());
    for (ordinal, created_at, msg) in tail {
        entries.push((ordinal, 0, ordinal, Entry::Row(ordinal, created_at, msg)));
    }
    for ev in control_events {
        entries.push((ev.after_ordinal, 1, ev.seq, Entry::Control(ev)));
    }
    entries.sort_by_key(|(anchor, is_control, tiebreak, _)| (*anchor, *is_control, *tiebreak));

    let mut items: Vec<ChatTranscriptItem> = Vec::new();
    let mut work = WorkAccumulator::default();
    // Start of the turn currently being folded — the most recent real user
    // message. A direct-answer turn persists its reasoning in the same row as
    // the answer (there are no intermediate rows to time against), so its
    // reconstructed work block spans from here to the answer's timestamp.
    let mut turn_started: Option<DateTime<Utc>> = None;

    // Whether the entry at index `i` is a `/stop` echo that ACTUALLY cancelled
    // an in-progress reply — i.e. its acknowledgement notice (the next entry,
    // same anchor + next seq) carries `STOP_CANCELLED_REPLY_LINE`. A no-op
    // `/stop` typed after a turn already finished says "Nothing in progress to
    // stop." instead, so it must NOT fold the completed answer into a
    // "Cancelled" block (that bug hid finished replies on reload).
    let is_cancelling_stop_echo: Vec<bool> = (0..entries.len())
        .map(|i| {
            let is_echo =
                matches!(entries.get(i), Some((_, _, _, Entry::Control(ev))) if is_stop_control_event(ev));
            is_echo
                && matches!(
                    entries.get(i + 1),
                    Some((_, _, _, Entry::Control(ev)))
                        if ev.kind == ControlEventKind::NoticeInfo
                            && ev.text.contains(STOP_CANCELLED_REPLY_LINE)
                )
        })
        .collect();
    // Whether the entry AFTER index `i` is such a cancelling `/stop` — lets the
    // final-answer arm recognise a cancelled partial (its trailing row) and
    // fold it into the work block instead of emitting an answer bubble.
    let next_is_cancelling_stop: Vec<bool> = (0..entries.len())
        .map(|i| is_cancelling_stop_echo.get(i + 1).copied().unwrap_or(false))
        .collect();

    for (idx, (_, _, _, entry)) in entries.into_iter().enumerate() {
        let (ordinal, created_at, msg) = match entry {
            Entry::Control(ev) => {
                // A control event interrupting an open work block bounds it:
                // for the common case — the `/stop` echo + notice right after
                // a cancelled turn's partial rows — the event instant is when
                // the work actually ended, where the fallback (last persisted
                // row) would undercount a turn stopped mid-LLM-call to `0s`.
                // A `/stop` that actually cancelled the reply marks the block
                // cancelled so the client labels it "Cancelled" rather than
                // plain "Worked Xs". A no-op `/stop` (nothing in progress)
                // leaves the block untouched.
                if is_cancelling_stop_echo[idx] {
                    work.cancelled = true;
                }
                let ended_at = ev.created_at;
                work.flush(&mut items, Some(ended_at));
                items.push(control_event_item(ev));
                // The event also ends the turn (`/stop`) or sits on a turn
                // boundary (`/compact`): a later turn with no user row of its
                // own (a subagent-notification fire) must not inherit the
                // interrupted turn's start.
                turn_started = None;
                continue;
            }
            Entry::Row(ordinal, created_at, msg) => (ordinal, created_at, msg),
        };
        match msg.role {
            Role::User if msg.from_user() => {
                work.flush(&mut items, None);
                turn_started = Some(created_at);
                if let Some(item) = message_item(
                    ordinal,
                    created_at,
                    "user",
                    &msg,
                    attachments_by_ordinal
                        .get(&ordinal)
                        .cloned()
                        .unwrap_or_default(),
                ) {
                    items.push(item);
                }
            }
            // Intermediate iteration: fold reasoning / narration / tool
            // calls into the open work block.
            Role::Assistant if msg.has_tool_use() => {
                if work.started.is_none() {
                    // Prefer the turn's start (the user row) over this row's
                    // own timestamp: the first intermediate row only lands
                    // after the first LLM call returns, so timing from it
                    // would drop that whole first thinking stretch from the
                    // `Worked Xs` label the live view counted. Falls back to
                    // the row when the user turn is off-page (or absent —
                    // cron fires).
                    work.started = Some(turn_started.unwrap_or(created_at));
                    work.ordinal = Some(ordinal);
                }
                work.last = Some(created_at);
                for block in &msg.content {
                    match block {
                        ContentBlock::Thinking { content, .. } => {
                            let text = thinking_text(content);
                            if !text.is_empty() {
                                work.steps.push(ChatWorkStep::reasoning(text));
                            }
                        }
                        ContentBlock::Text(t) if !t.trim().is_empty() => {
                            work.steps.push(ChatWorkStep::prose(t.clone()));
                        }
                        ContentBlock::ToolUse {
                            id, name, input, ..
                        } => {
                            work.pending_tools.insert(id.clone(), work.steps.len());
                            work.steps
                                .push(ChatWorkStep::tool(name.clone(), tool_label(input)));
                        }
                        _ => {}
                    }
                }
            }
            // Final answer (tool-free row): close the work block, then the
            // reply bubble lands below it. A direct-answer turn (no tool
            // iterations, so nothing accumulated yet) still carries its
            // reasoning in this same row; rebuild a single-step work block
            // from it so a reload shows the same `Worked Xs` + reasoning the
            // tool path produces, rather than dropping the thinking on the
            // floor in `message_item`.
            //
            // A turn cut short by a cancelling `/stop` (the next entry is the
            // cancelling echo) takes the SAME shape: its reasoning folds into
            // the work block and its partial text lands as a reply bubble —
            // the agent's cut-short final output stays a bubble rather than
            // being folded inside the work card. The only difference is the
            // block is flagged `cancelled` so it collapses to "Cancelled", and
            // the model-facing cancelled-turn marker is stripped from the
            // bubble. A no-op `/stop` after a finished turn does NOT match
            // `next_is_cancelling_stop`, so a completed answer is never marked.
            Role::Assistant => {
                if work.started.is_none() {
                    for block in &msg.content {
                        if let ContentBlock::Thinking { content, .. } = block {
                            let text = thinking_text(content);
                            if text.is_empty() {
                                continue;
                            }
                            if work.started.is_none() {
                                // Same page-boundary fallback as the tool
                                // path: a missing user row degrades to this
                                // row's own timestamp instead of a `None`
                                // that flush would paper over with `now()`.
                                work.started = Some(turn_started.unwrap_or(created_at));
                                work.ordinal = Some(ordinal);
                            }
                            work.steps.push(ChatWorkStep::reasoning(text));
                        }
                    }
                }
                let cancelled = next_is_cancelling_stop[idx];
                if cancelled {
                    work.cancelled = true;
                }
                work.flush(&mut items, Some(created_at));
                if let Some(mut item) = message_item(
                    ordinal,
                    created_at,
                    "assistant",
                    &msg,
                    attachments_by_ordinal
                        .get(&ordinal)
                        .cloned()
                        .unwrap_or_default(),
                ) {
                    if cancelled {
                        // Drop the model-facing cancelled-turn marker so the
                        // salvaged reply renders as clean partial output.
                        item.text =
                            baybo_context::prompts::cancelled_turn::strip_marker(&item.text)
                                .to_string();
                    }
                    // A marker-only salvage (thinking-only cancelled turn)
                    // leaves nothing to show once stripped — no empty bubble.
                    if !item.text.is_empty() || item.has_attachments {
                        items.push(item);
                    }
                }
                // Turn boundary: a later turn that has no user row on this
                // page (a cron fire) must not inherit this turn's start.
                turn_started = None;
            }
            Role::Tool => {
                work.last = Some(created_at);
                for block in &msg.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = block
                        && let Some(&idx) = work.pending_tools.get(tool_use_id)
                        && let Some(step) = work.steps.get_mut(idx)
                    {
                        step.tool_status = Some(tool_result_status(content));
                        step.tool_summary = Some(summarize_tool_result(content));
                    }
                }
            }
            _ => {}
        }
    }
    // Trailing block: a turn whose final answer is beyond this page, or
    // that ended on a tool with no narrated reply. The work item inherits
    // its first intermediate row's ordinal, so when it lands as the very
    // last item the client seeds its WS catch-up cursor slightly behind
    // the true tail — harmless, since the re-replayed rows are dropped by
    // the message-only catch-up filter. A turn straddling the page
    // boundary likewise reconstructs partially until an older page loads.
    //
    // Fold the in-flight turn's still-streaming progress (reasoning / tool
    // steps the live channel buffered but hasn't persisted) onto the end of the
    // trailing block — so a tab loading mid-turn shows what was thought before
    // it joined. For a turn still in its first iteration there's no persisted
    // intermediate row, so seed the block's ordinal from the newest message.
    let has_in_flight = !in_flight_steps.is_empty();
    work.steps.extend(in_flight_steps);
    if has_in_flight && work.ordinal.is_none() {
        work.ordinal = Some(last_ordinal);
    }
    // When a turn is still in flight, align this trailing block's start with
    // the live `TurnState`'s `started_at` (the job start instant) rather than
    // the first message's timestamp. Both are computed from
    // `active_turn_started_at`, so they match exactly — which is what lets a
    // reloading tab *reopen* this block on the next `turn_state{active}`
    // (`workStartedAt === startedAt`) instead of opening a second one.
    if let Some(start) = active_turn_started
        && !work.steps.is_empty()
    {
        work.started = Some(start);
    }
    work.flush(&mut items, None);
    items
}

/// Flatten a user / final-assistant message into a `message` item, or
/// `None` when it has neither text nor attachments (e.g. an assistant turn
/// that produced only tool calls) — such a row would render as an empty
/// bubble.
fn message_item(
    ordinal: i64,
    created_at: DateTime<Utc>,
    role: &str,
    msg: &ChatMessage,
    attachments: Vec<ChatAttachment>,
) -> Option<ChatTranscriptItem> {
    let mut text = String::new();
    let mut has_attachments = false;
    for block in &msg.content {
        match block {
            ContentBlock::Text(t) => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
            ContentBlock::Image { .. } | ContentBlock::Audio { .. } | ContentBlock::File { .. } => {
                has_attachments = true;
            }
            _ => {}
        }
    }
    if text.is_empty() && !has_attachments {
        return None;
    }
    Some(ChatTranscriptItem {
        id: format!("m{ordinal}"),
        ordinal: Some(ordinal),
        kind: TranscriptItemKind::Message,
        role: role.to_owned(),
        text,
        has_attachments,
        platform_msg_id: msg.platform_msg_id().to_string(),
        attachments,
        created_at,
        steps: Vec::new(),
        work_started_at: None,
        work_ended_at: None,
        cancelled: false,
        notice_level: None,
    })
}

/// Map a [`ControlEvent`] (an out-of-band slash-command echo or notice, stored
/// outside `session_messages`) into a transcript item: a `command` renders as a
/// user bubble (what the user typed), a `notice_*` as a colored notice bar.
/// Control events are NOT ordinal-addressed — the item carries no `ordinal`
/// and is keyed by its stable `n<seq>` row id, which is also the client's
/// redelivery dedup key (sync re-delivers events anchored exactly at the
/// cursor). Position comes from the `after_ordinal` anchor in
/// `reconstruct_transcript`; the client reads page bounds from
/// `ChatSessionDetail::{oldest,newest}_ordinal`.
fn control_event_item(ev: ControlEvent) -> ChatTranscriptItem {
    // `notice_level()` is `Some` for a notice kind, `None` for a command echo.
    let level = ev.kind.notice_level();
    let (kind, role) = match level {
        Some(_) => (TranscriptItemKind::Notice, String::new()),
        None => (TranscriptItemKind::Message, "user".to_owned()),
    };
    ChatTranscriptItem {
        id: format!("n{}", ev.seq),
        ordinal: None,
        kind,
        role,
        text: ev.text,
        has_attachments: false,
        platform_msg_id: String::new(),
        attachments: Vec::new(),
        created_at: ev.created_at,
        steps: Vec::new(),
        work_started_at: None,
        work_ended_at: None,
        cancelled: false,
        notice_level: level.map(str::to_owned),
    }
}

/// Concatenate the visible text of a model thinking block (redacted
/// reasoning carries no display text and is skipped).
fn thinking_text(content: &[ThinkingContent]) -> String {
    let mut out = String::new();
    for c in content {
        let part = match c {
            ThinkingContent::Text { text, .. } | ThinkingContent::Summary { text } => text.as_str(),
            ThinkingContent::Redacted { .. } => continue,
        };
        if part.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(part);
    }
    out
}

/// Best-effort short label for a tool call, pulled from a common input key
/// (path / command / url / query). Stands in for the live `progress_label`,
/// which needs the tool registry that isn't on the read path. `None` when
/// nothing recognizable is present.
fn tool_label(input: &serde_json::Value) -> Option<String> {
    const KEYS: [&str; 6] = ["command", "url", "path", "file_path", "query", "pattern"];
    let obj = input.as_object()?;
    for key in KEYS {
        if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
            let label = collapse_ws(s);
            if !label.is_empty() {
                return Some(truncate(&label, 80));
            }
        }
    }
    None
}

/// Re-derive a one-line summary of a persisted tool result. The exact live
/// summary (line counts, `exit 0`) isn't persisted, so this strips the
/// `<tool_output>` envelope and returns a short, whitespace-collapsed
/// snippet of the result body.
fn summarize_tool_result(content: &str) -> String {
    truncate(&collapse_ws(strip_tool_output_envelope(content)), 140)
}

/// Best-effort `ok` / `error` / `denied` status for a persisted tool
/// result. The structured outcome isn't stored, so this keys off the
/// agent's own result-formatting prefixes (`runtime::agent_loop` writes
/// `Error: …` for failures and a fixed `The user explicitly denied
/// permission …` message for denied calls) — enough for reload to
/// color-code failures the way the live view did.
fn tool_result_status(content: &str) -> String {
    let inner = strip_tool_output_envelope(content);
    let inner = inner.trim_start();
    if inner.starts_with("The user explicitly denied permission") {
        "denied".to_owned()
    } else if inner.starts_with("Error:") {
        "error".to_owned()
    } else {
        "ok".to_owned()
    }
}

/// Strip the `<tool_output …> … </tool_output>` wrapper the agent puts
/// around untrusted tool results before they enter the transcript. Returns
/// the input unchanged when it isn't wrapped.
fn strip_tool_output_envelope(content: &str) -> &str {
    let Some(rest) = content.trim_start().strip_prefix("<tool_output") else {
        return content;
    };
    let Some((_, body)) = rest.split_once('>') else {
        return content;
    };
    match body.rfind("</tool_output") {
        Some(close) => body[..close].trim(),
        None => body.trim(),
    }
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0)
            .single()
            .expect("valid timestamp")
    }
    fn text(s: &str) -> ContentBlock {
        ContentBlock::Text(s.to_owned())
    }
    fn thinking(s: &str) -> ContentBlock {
        ContentBlock::Thinking {
            id: None,
            content: vec![ThinkingContent::Text {
                text: s.to_owned(),
                signature: None,
            }],
        }
    }
    fn tool_use(id: &str, name: &str, input: serde_json::Value) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.to_owned(),
            name: name.to_owned(),
            input,
            signature: None,
        }
    }
    fn ctl(
        seq: i64,
        after: i64,
        kind: baybo_model::ControlEventKind,
        body: &str,
        secs: i64,
    ) -> ControlEvent {
        ControlEvent {
            seq,
            after_ordinal: after,
            kind,
            text: body.to_owned(),
            created_at: ts(secs),
        }
    }

    #[test]
    fn reconstruct_groups_tool_turn_into_work_block_before_answer() {
        let tail = vec![
            (2, ts(2), ChatMessage::user(vec![text("write hello")])),
            (
                3,
                ts(3),
                ChatMessage::assistant(vec![
                    thinking("I'll write it"),
                    text("let me try"),
                    tool_use("c1", "Write", serde_json::json!({"path": "/tmp/x"})),
                ]),
            ),
            (
                4,
                ts(7),
                ChatMessage::tool_result(
                    "c1".to_owned(),
                    "<tool_output name=\"Write\">Error: nope</tool_output>".to_owned(),
                ),
            ),
            (
                5,
                ts(9),
                ChatMessage::assistant(vec![text("done, sort of")]),
            ),
        ];
        let items = reconstruct_transcript(tail, Vec::new(), None, Vec::new());
        assert_eq!(items.len(), 3);

        assert!(matches!(items[0].kind, TranscriptItemKind::Message));
        assert_eq!(items[0].role, "user");
        assert_eq!(items[0].text, "write hello");

        let work = &items[1];
        assert!(matches!(work.kind, TranscriptItemKind::Work));
        assert_eq!(
            work.ordinal,
            Some(3),
            "work block inherits first intermediate ordinal"
        );
        assert_eq!(work.id, "w3", "work row id derives from that ordinal");
        assert_eq!(
            work.work_started_at,
            Some(ts(2)),
            "starts at the user turn, not the first persisted iteration"
        );
        assert_eq!(work.work_ended_at, Some(ts(9)), "ends at the final reply");
        assert_eq!(work.steps.len(), 3);
        assert!(matches!(work.steps[0].kind, WorkStepKind::Reasoning));
        assert_eq!(work.steps[0].text, "I'll write it");
        assert!(matches!(work.steps[1].kind, WorkStepKind::Prose));
        assert_eq!(work.steps[1].text, "let me try");
        assert!(matches!(work.steps[2].kind, WorkStepKind::Tool));
        assert_eq!(work.steps[2].tool.as_deref(), Some("Write"));
        assert_eq!(work.steps[2].tool_label.as_deref(), Some("/tmp/x"));
        assert_eq!(work.steps[2].tool_status.as_deref(), Some("error"));
        assert_eq!(work.steps[2].tool_summary.as_deref(), Some("Error: nope"));

        assert!(matches!(items[2].kind, TranscriptItemKind::Message));
        assert_eq!(items[2].role, "assistant");
        assert_eq!(items[2].text, "done, sort of");
    }

    #[test]
    fn reconstruct_interleaves_control_events_by_anchor() {
        use baybo_model::ControlEventKind::{Command, NoticeInfo, NoticeWarn};
        // A /stop'd tool turn: user (ord 2) → tool-using assistant (ord 3) →
        // tool result (ord 4), with no final answer. Control events: a warn
        // notice anchored before any row (after_ordinal -1), and the /stop echo
        // + its info notice both anchored after the tail (after_ordinal 4).
        let tail = vec![
            (2, ts(2), ChatMessage::user(vec![text("go")])),
            (
                3,
                ts(3),
                ChatMessage::assistant(vec![tool_use(
                    "c1",
                    "Bash",
                    serde_json::json!({"command": "sleep 99"}),
                )]),
            ),
            (
                4,
                ts(4),
                ChatMessage::tool_result("c1".to_owned(), "ok".to_owned()),
            ),
        ];
        // The acknowledgement carries the real cancellation line so reconstruct
        // recognises this `/stop` as one that actually cancelled the turn.
        let stop_notice = format!("Stopped.\n{}", baybo_channels::STOP_CANCELLED_REPLY_LINE);
        // Deliberately out of sorted order to prove reconstruct sorts them.
        let events = vec![
            ctl(2, 4, NoticeInfo, &stop_notice, 5),
            ctl(0, -1, NoticeWarn, "early", 1),
            ctl(1, 4, Command, "/stop", 5),
        ];
        let items = reconstruct_transcript(tail, events, None, Vec::new());

        // early notice, user, work block, /stop echo, /stop notice
        assert_eq!(items.len(), 5);

        // after_ordinal -1 sorts before every row.
        assert!(matches!(items[0].kind, TranscriptItemKind::Notice));
        assert_eq!(items[0].text, "early");
        assert_eq!(items[0].notice_level.as_deref(), Some("warn"));

        assert!(matches!(items[1].kind, TranscriptItemKind::Message));
        assert_eq!(items[1].role, "user");
        assert_eq!(items[1].text, "go");

        // The tool turn folds into one work block, flushed before the control
        // events anchored after it — and *bounded* by them: the stop instant
        // is when the work ended, not the last persisted row (which would
        // report a turn stopped mid-LLM-call as `Worked 0s`).
        assert!(matches!(items[2].kind, TranscriptItemKind::Work));
        assert!(
            items[2].cancelled,
            "a /stop'd turn's work block is marked cancelled"
        );
        assert_eq!(items[2].work_started_at, Some(ts(2)));
        assert_eq!(
            items[2].work_ended_at,
            Some(ts(5)),
            "a /stop'd turn's work block ends at the stop instant"
        );

        // Same anchor (4): seq orders the echo (Command → user bubble) before
        // the notice.
        assert!(matches!(items[3].kind, TranscriptItemKind::Message));
        assert_eq!(items[3].role, "user");
        assert_eq!(items[3].text, "/stop");
        assert_eq!(items[3].notice_level, None);

        assert!(matches!(items[4].kind, TranscriptItemKind::Notice));
        assert_eq!(items[4].text, stop_notice);
        assert_eq!(items[4].notice_level.as_deref(), Some("info"));

        // Control items are not ordinal-addressed: no ordinal, and a stable
        // `n<seq>` row id that doubles as the redelivery dedup key.
        for (ctl_item, want_id) in [(&items[0], "n0"), (&items[3], "n1"), (&items[4], "n2")] {
            assert_eq!(ctl_item.ordinal, None, "control items carry no ordinal");
            assert_eq!(ctl_item.id, want_id, "control row id is n<seq>");
        }
    }

    #[test]
    fn reconstruct_stopped_partial_turn_salvages_reply_as_bubble_below_cancelled_block() {
        use baybo_model::ControlEventKind::{Command, NoticeInfo};
        // A turn cancelled mid-LLM-call: the loop persisted a partial assistant
        // row (reasoning + partial answer text, no tool calls) before aborting,
        // then the `/stop` echo + notice anchored right after it (the ordering
        // the `/stop` settle-wait guarantees). The persisted partial carries
        // the model-facing cancelled-turn marker, which display must strip.
        let partial = format!(
            "a b-tree is{}",
            baybo_context::prompts::cancelled_turn::SUFFIX
        );
        let tail = vec![
            (2, ts(2), ChatMessage::user(vec![text("explain b-trees")])),
            (
                3,
                ts(3),
                ChatMessage::assistant(vec![thinking("weighing the options"), text(&partial)]),
            ),
        ];
        let stop_notice = format!("Stopped.\n{}", baybo_channels::STOP_CANCELLED_REPLY_LINE);
        let events = vec![
            ctl(1, 3, Command, "/stop", 5),
            ctl(2, 3, NoticeInfo, &stop_notice, 5),
        ];
        let items = reconstruct_transcript(tail, events, None, Vec::new());

        // user, cancelled work block, the salvaged reply bubble, /stop echo,
        // /stop notice — the cut-short reply is its OWN bubble below the
        // collapsed "Cancelled" block, not folded inside it.
        assert_eq!(items.len(), 5, "got {items:?}");
        assert!(matches!(items[0].kind, TranscriptItemKind::Message));
        assert_eq!(items[0].role, "user");

        let work = &items[1];
        assert!(matches!(work.kind, TranscriptItemKind::Work));
        assert!(
            work.cancelled,
            "a /stop'd partial turn's work block is cancelled"
        );
        // Reasoning stays in the block; the partial answer text does NOT.
        assert_eq!(work.steps.len(), 1, "only reasoning folds into the block");
        assert!(matches!(work.steps[0].kind, WorkStepKind::Reasoning));
        assert_eq!(work.steps[0].text, "weighing the options");

        let reply = &items[2];
        assert!(matches!(reply.kind, TranscriptItemKind::Message));
        assert_eq!(reply.role, "assistant");
        assert_eq!(
            reply.text, "a b-tree is",
            "the salvaged reply renders as a bubble with the model-facing marker stripped"
        );

        assert_eq!(items[3].text, "/stop");
        assert!(matches!(items[4].kind, TranscriptItemKind::Notice));
    }

    #[test]
    fn reconstruct_stopped_thinking_only_turn_has_no_reply_bubble() {
        use baybo_model::ControlEventKind::{Command, NoticeInfo};
        // Cancelled before any answer text streamed — only reasoning was
        // salvaged (so the persisted partial is a marker-only block). The
        // cancelled work block carries the reasoning; there is no reply bubble.
        let tail = vec![
            (2, ts(2), ChatMessage::user(vec![text("explain b-trees")])),
            (
                3,
                ts(3),
                ChatMessage::assistant(vec![
                    thinking("weighing the options"),
                    text(&baybo_context::prompts::cancelled_turn::marker_block_text()),
                ]),
            ),
        ];
        let stop_notice = format!("Stopped.\n{}", baybo_channels::STOP_CANCELLED_REPLY_LINE);
        let events = vec![
            ctl(1, 3, Command, "/stop", 5),
            ctl(2, 3, NoticeInfo, &stop_notice, 5),
        ];
        let items = reconstruct_transcript(tail, events, None, Vec::new());

        assert!(
            !items
                .iter()
                .any(|i| matches!(i.kind, TranscriptItemKind::Message) && i.role == "assistant"),
            "a marker-only (thinking-only) salvage must not render an empty reply bubble: {items:?}"
        );
        let work = items
            .iter()
            .find(|i| matches!(i.kind, TranscriptItemKind::Work))
            .expect("cancelled work block");
        assert!(work.cancelled);
        assert_eq!(work.steps.len(), 1, "the reasoning is preserved");
    }

    #[test]
    fn reconstruct_completed_answer_then_noop_stop_keeps_the_answer() {
        use baybo_model::ControlEventKind::{Command, NoticeInfo};
        // A turn that FINISHED (full answer), then the user typed `/stop` — a
        // no-op (its notice says "Nothing in progress to stop."). The completed
        // answer must render as its bubble, NOT get folded into a "Cancelled"
        // work block (the regression that hid finished replies on reload).
        let tail = vec![
            (2, ts(2), ChatMessage::user(vec![text("explain b-trees")])),
            (
                3,
                ts(3),
                ChatMessage::assistant(vec![text("A b-tree is a balanced, sorted tree …")]),
            ),
        ];
        let events = vec![
            ctl(1, 3, Command, "/stop", 5),
            // No-op stop: nothing was in progress, so no cancellation line.
            ctl(2, 3, NoticeInfo, "Nothing in progress to stop.", 5),
        ];
        let items = reconstruct_transcript(tail, events, None, Vec::new());

        // The finished answer survives as an assistant bubble; nothing is
        // marked cancelled.
        let answer = items
            .iter()
            .find(|i| matches!(i.kind, TranscriptItemKind::Message) && i.role == "assistant")
            .expect("the completed answer must still render as a bubble");
        assert!(answer.text.contains("A b-tree is"), "got {:?}", answer.text);
        assert!(
            !items.iter().any(|i| i.cancelled),
            "a no-op /stop after a finished turn must not mark anything cancelled: {items:?}"
        );
    }

    #[test]
    fn reconstruct_in_flight_turn_aligns_work_start_with_live_turn_state() {
        // An in-flight turn (tool call persisted, no final answer yet). With an
        // active turn, the trailing work block must start at the live
        // TurnState instant (ts(9), the job start) — NOT the user message's
        // timestamp (ts(2)) — so a reloading tab reopens THIS block on the
        // next `turn_state{active}` (start-match) instead of opening a second.
        let tail = vec![
            (2, ts(2), ChatMessage::user(vec![text("go")])),
            (
                3,
                ts(3),
                ChatMessage::assistant(vec![tool_use(
                    "c1",
                    "Bash",
                    serde_json::json!({"command": "sleep 99"}),
                )]),
            ),
        ];
        let aligned = reconstruct_transcript(tail.clone(), Vec::new(), Some(ts(9)), Vec::new());
        let work = aligned
            .iter()
            .find(|i| matches!(i.kind, TranscriptItemKind::Work))
            .expect("in-flight work block");
        assert_eq!(
            work.work_started_at,
            Some(ts(9)),
            "in-flight block starts at the live TurnState instant, not the message time"
        );

        // Without an active turn (older page / no in-flight turn), the start
        // stays the message-derived value — no override.
        let bare = reconstruct_transcript(tail, Vec::new(), None, Vec::new());
        let work = bare
            .iter()
            .find(|i| matches!(i.kind, TranscriptItemKind::Work))
            .expect("work block");
        assert_eq!(
            work.work_started_at,
            Some(ts(2)),
            "no active turn → message-time start"
        );
    }

    #[test]
    fn reconstruct_folds_in_flight_steps_into_the_trailing_block() {
        // A turn still in its first iteration: only the user message persisted,
        // the reasoning is still streaming (buffered by the live channel). The
        // in-flight steps must surface as the trailing work block, started at
        // the live TurnState instant so a reload reopens it.
        let tail = vec![(2, ts(2), ChatMessage::user(vec![text("explain b-trees")]))];
        let in_flight = vec![
            ChatWorkStep::reasoning("weighing the options".into()),
            ChatWorkStep::tool("Bash".into(), Some("ls".into())),
        ];
        let items = reconstruct_transcript(tail, Vec::new(), Some(ts(9)), in_flight);

        let work = items
            .iter()
            .find(|i| matches!(i.kind, TranscriptItemKind::Work))
            .expect("in-flight work block");
        assert_eq!(
            work.work_started_at,
            Some(ts(9)),
            "in-flight block starts at the live TurnState instant"
        );
        assert!(!work.cancelled, "a still-streaming turn is not cancelled");
        assert_eq!(
            work.steps.len(),
            2,
            "reasoning + tool folded in: {:?}",
            work.steps
        );
        assert!(matches!(work.steps[0].kind, WorkStepKind::Reasoning));
        assert_eq!(work.steps[0].text, "weighing the options");
        assert!(matches!(work.steps[1].kind, WorkStepKind::Tool));
        // No assistant answer bubble — the turn hasn't replied yet.
        assert!(
            !items
                .iter()
                .any(|i| matches!(i.kind, TranscriptItemKind::Message) && i.role == "assistant"),
            "an in-flight turn has no answer bubble yet: {items:?}"
        );
    }

    #[test]
    fn reconstruct_control_event_mid_block_splits_the_work() {
        use baybo_model::ControlEventKind::NoticeInfo;
        // A control event anchored at a row *inside* a tool turn (after_ordinal
        // 3, the intermediate assistant row) flushes the work accumulated so far
        // before the final answer.
        let tail = vec![
            (2, ts(2), ChatMessage::user(vec![text("go")])),
            (
                3,
                ts(3),
                ChatMessage::assistant(vec![tool_use(
                    "c1",
                    "Bash",
                    serde_json::json!({"command": "echo hi"}),
                )]),
            ),
            (4, ts(6), ChatMessage::assistant(vec![text("done")])),
        ];
        let events = vec![ctl(0, 3, NoticeInfo, "midway", 4)];
        let items = reconstruct_transcript(tail, events, None, Vec::new());

        // user, work (the c1 tool step), notice, answer
        assert_eq!(items.len(), 4);
        assert!(matches!(items[0].kind, TranscriptItemKind::Message));
        assert!(matches!(items[1].kind, TranscriptItemKind::Work));
        assert!(matches!(items[2].kind, TranscriptItemKind::Notice));
        assert_eq!(items[2].text, "midway");
        assert!(matches!(items[3].kind, TranscriptItemKind::Message));
        assert_eq!(items[3].role, "assistant");
        assert_eq!(items[3].text, "done");
    }

    #[test]
    fn reconstruct_turn_without_user_row_does_not_inherit_prior_start() {
        // A turn with no user row on the page (a cron fire, or a turn whose
        // user row fell off the page boundary) times its work block from its
        // own first intermediate row — never from a *previous* turn's user
        // message, which would inflate `Worked Xs` by the idle gap.
        let tail = vec![
            (2, ts(2), ChatMessage::user(vec![text("hi")])),
            (3, ts(4), ChatMessage::assistant(vec![text("hello")])),
            (
                4,
                ts(1000),
                ChatMessage::assistant(vec![tool_use(
                    "c1",
                    "Bash",
                    serde_json::json!({"command": "echo cron"}),
                )]),
            ),
            (5, ts(1003), ChatMessage::assistant(vec![text("cron done")])),
        ];
        let items = reconstruct_transcript(tail, Vec::new(), None, Vec::new());
        // user, answer, work, answer
        assert_eq!(items.len(), 4);
        let work = &items[2];
        assert!(matches!(work.kind, TranscriptItemKind::Work));
        assert_eq!(
            work.work_started_at,
            Some(ts(1000)),
            "no user row in this turn → falls back to its own first row"
        );
        assert_eq!(work.work_ended_at, Some(ts(1003)));
    }

    #[test]
    fn reconstruct_no_user_row_turn_after_stop_does_not_inherit_start() {
        use baybo_model::ControlEventKind::{Command, NoticeInfo};
        // A /stop'd turn ends at its control events; the next turn on the
        // page has no user row (a subagent-notification fire). Its work
        // block must time from its own first row, not the stopped turn's
        // user message.
        let tail = vec![
            (2, ts(10), ChatMessage::user(vec![text("go")])),
            (
                3,
                ts(12),
                ChatMessage::assistant(vec![tool_use(
                    "c1",
                    "Bash",
                    serde_json::json!({"command": "sleep 99"}),
                )]),
            ),
            (
                4,
                ts(5000),
                ChatMessage::assistant(vec![tool_use(
                    "c2",
                    "Read",
                    serde_json::json!({"path": "/tmp/x"}),
                )]),
            ),
            (5, ts(5003), ChatMessage::assistant(vec![text("done")])),
        ];
        let events = vec![
            ctl(0, 3, Command, "/stop", 15),
            ctl(1, 3, NoticeInfo, "Stopped.", 15),
        ];
        let items = reconstruct_transcript(tail, events, None, Vec::new());

        // user, work (stopped), /stop echo, notice, work (notification), answer
        assert_eq!(items.len(), 6);
        let stopped = &items[1];
        assert!(matches!(stopped.kind, TranscriptItemKind::Work));
        assert_eq!(stopped.work_started_at, Some(ts(10)));
        assert_eq!(stopped.work_ended_at, Some(ts(15)));

        let notification = &items[4];
        assert!(matches!(notification.kind, TranscriptItemKind::Work));
        assert_eq!(
            notification.work_started_at,
            Some(ts(5000)),
            "must not inherit the stopped turn's user-row start"
        );
        assert_eq!(notification.work_ended_at, Some(ts(5003)));
    }

    #[test]
    fn reconstruct_direct_answer_with_thinking_gets_work_block() {
        // A turn that thinks then answers with no tool calls keeps both rows
        // logically: a `Worked Xs` work block (spanning the user turn → the
        // answer) carrying the reasoning, then the answer bubble.
        let tail = vec![
            (2, ts(2), ChatMessage::user(vec![text("1+1?")])),
            (
                3,
                ts(15),
                ChatMessage::assistant(vec![thinking("let me add"), text("2")]),
            ),
        ];
        let items = reconstruct_transcript(tail, Vec::new(), None, Vec::new());
        assert_eq!(items.len(), 3);

        assert!(matches!(items[0].kind, TranscriptItemKind::Message));
        assert_eq!(items[0].role, "user");

        let work = &items[1];
        assert!(matches!(work.kind, TranscriptItemKind::Work));
        assert_eq!(work.ordinal, Some(3), "shares the answer row's ordinal");
        assert_eq!(
            work.work_started_at,
            Some(ts(2)),
            "spans from the user turn that prompted it"
        );
        assert_eq!(work.work_ended_at, Some(ts(15)));
        assert_eq!(work.steps.len(), 1);
        assert!(matches!(work.steps[0].kind, WorkStepKind::Reasoning));
        assert_eq!(work.steps[0].text, "let me add");

        assert!(matches!(items[2].kind, TranscriptItemKind::Message));
        assert_eq!(items[2].role, "assistant");
        assert_eq!(items[2].text, "2");
    }

    #[test]
    fn reconstruct_direct_answer_without_thinking_stays_bare() {
        // No reasoning to surface → no empty work block, just the answer.
        let tail = vec![
            (2, ts(2), ChatMessage::user(vec![text("hi")])),
            (3, ts(3), ChatMessage::assistant(vec![text("hello")])),
        ];
        let items = reconstruct_transcript(tail, Vec::new(), None, Vec::new());
        assert_eq!(items.len(), 2);
        assert!(
            items
                .iter()
                .all(|i| matches!(i.kind, TranscriptItemKind::Message))
        );
        assert_eq!(items[1].text, "hello");
    }

    #[test]
    fn reconstruct_flushes_trailing_work_block_without_final_answer() {
        let tail = vec![
            (2, ts(2), ChatMessage::user(vec![text("go")])),
            (
                3,
                ts(3),
                ChatMessage::assistant(vec![tool_use(
                    "c1",
                    "Bash",
                    serde_json::json!({"command": "ls"}),
                )]),
            ),
            (
                4,
                ts(5),
                ChatMessage::tool_result("c1".to_owned(), "ok output".to_owned()),
            ),
        ];
        let items = reconstruct_transcript(tail, Vec::new(), None, Vec::new());
        assert_eq!(items.len(), 2);
        let work = &items[1];
        assert!(matches!(work.kind, TranscriptItemKind::Work));
        assert_eq!(work.work_ended_at, Some(ts(5)), "ends at last seen row");
        assert_eq!(work.steps.len(), 1);
        assert_eq!(work.steps[0].tool_status.as_deref(), Some("ok"));
        assert_eq!(work.steps[0].tool_summary.as_deref(), Some("ok output"));
    }

    #[test]
    fn reconstruct_drops_internal_rows() {
        let tail = vec![
            (1, ts(1), ChatMessage::system(vec![text("system prompt")])),
            (2, ts(2), ChatMessage::agent_context(vec![text("injected")])),
            (3, ts(3), ChatMessage::user(vec![text("hi")])),
            (4, ts(4), ChatMessage::assistant(vec![text("hello")])),
        ];
        let items = reconstruct_transcript(tail, Vec::new(), None, Vec::new());
        assert_eq!(items.len(), 2);
        assert!(
            items
                .iter()
                .all(|i| matches!(i.kind, TranscriptItemKind::Message))
        );
        assert_eq!(items[0].text, "hi");
        assert_eq!(items[1].text, "hello");
    }

    #[test]
    fn reconstruct_leaves_summary_none_when_tool_result_absent() {
        let tail = vec![(
            3,
            ts(3),
            ChatMessage::assistant(vec![tool_use(
                "c1",
                "Bash",
                serde_json::json!({"command": "ls"}),
            )]),
        )];
        let items = reconstruct_transcript(tail, Vec::new(), None, Vec::new());
        assert_eq!(items.len(), 1);
        assert!(items[0].steps[0].tool_summary.is_none());
        assert!(items[0].steps[0].tool_status.is_none());
    }

    #[test]
    fn message_item_skips_empty_keeps_text() {
        let only_tool = ChatMessage::assistant(vec![tool_use("c1", "X", serde_json::json!({}))]);
        assert!(message_item(1, ts(1), "assistant", &only_tool, Vec::new()).is_none());

        let multi = ChatMessage::assistant(vec![text("a"), text("b")]);
        let item = message_item(1, ts(1), "assistant", &multi, Vec::new()).expect("non-empty");
        assert_eq!(item.text, "a\nb");
        assert!(!item.has_attachments);
    }

    #[test]
    fn thinking_text_joins_and_skips_redacted() {
        let blocks = vec![
            ThinkingContent::Text {
                text: "step 1".to_owned(),
                signature: None,
            },
            ThinkingContent::Redacted {
                data: "opaque".to_owned(),
            },
            ThinkingContent::Summary {
                text: "tl;dr".to_owned(),
            },
        ];
        assert_eq!(thinking_text(&blocks), "step 1\ntl;dr");
    }

    #[test]
    fn tool_label_pulls_first_known_key() {
        assert_eq!(
            tool_label(&serde_json::json!({"command": "ls -la"})).as_deref(),
            Some("ls -la")
        );
        assert_eq!(
            tool_label(&serde_json::json!({"path": "/a/b"})).as_deref(),
            Some("/a/b")
        );
        assert_eq!(tool_label(&serde_json::json!({"other": "x"})), None);
        assert_eq!(tool_label(&serde_json::json!("not an object")), None);
    }

    #[test]
    fn strip_envelope_and_status() {
        assert_eq!(
            strip_tool_output_envelope("<tool_output name=\"Read\">200 lines</tool_output>"),
            "200 lines"
        );
        assert_eq!(strip_tool_output_envelope("no envelope"), "no envelope");

        assert_eq!(
            tool_result_status("<tool_output name=\"x\">Error: boom</tool_output>"),
            "error"
        );
        assert_eq!(
            tool_result_status("The user explicitly denied permission for tool 'x'."),
            "denied"
        );
        assert_eq!(tool_result_status("all good"), "ok");
    }
}
