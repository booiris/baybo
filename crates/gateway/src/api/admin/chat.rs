//! `/v1/chat/*` — admin-side endpoints for the web chat page.
//!
//! Surface:
//!
//! * `POST /v1/chat/sessions` — create a new session (channel=http),
//!   mint a short-lived channel-token bound to that session's web tab,
//!   return both.
//! * `GET /v1/chat/sessions` — list the http channel's sessions
//!   (newest first). Hidden sessions are filtered out unless the
//!   `include_hidden=true` query is set.
//! * `GET /v1/chat/sessions/:id` — session detail + transcript history.
//! * `DELETE /v1/chat/sessions/:id` — **hide** the session from the
//!   chat list. The row, transcript, and channel-token stay live;
//!   admin / trace surfaces still see it. Reversible via
//!   `POST /v1/chat/sessions/:id/unhide`.
//! * `POST /v1/chat/sessions/:id/unhide` — undo the hide.
//! * `POST /v1/chat/sessions/:id/token` — refresh the channel-token
//!   (drop old, mint new). Used by the web client when its existing
//!   token's lifetime is close to expiring.
//! * `GET /v1/chat/slash-manifest` — list of slash commands the input
//!   composer's `/`-autocomplete should surface.
//!
//! The web client uses the returned `channel_token` to authenticate
//! against `/v1/channel-ws`, which the admin listener co-hosts on its
//! public bind (so the browser can reach it from the same origin that
//! served the web bundle) alongside the loopback channel listener.
//! The channel-auth middleware turns the token into
//! [`crate::auth::AuthedClient::Web`].

use std::collections::HashMap;

use aura_channels::wire::{SessionPatch, SlashCommandSpec};
use aura_model::{
    ChannelType, ChatMessage, ContentBlock, MessageSource, Role, Session, SessionId,
    ThinkingContent, TriggerSource, User,
};
use axum::Json;
use axum::extract::{Path, Query, State};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::api::dto::{ErrorBody, ListResponse};
use crate::auth::{
    CHANNEL_TOKEN_HEADER, ClientIdentity, WEB_CLIENT_LABEL_PREFIX, WEB_OPERATOR_USER_ID,
};
use crate::channel::StashedTokenHandle;
use crate::server::AdminState;
use crate::{GatewayError, Result};

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(create_session))
        .routes(routes!(list_sessions))
        .routes(routes!(get_session))
        .routes(routes!(delete_session))
        .routes(routes!(unhide_session))
        .routes(routes!(refresh_session_token))
        .routes(routes!(slash_manifest))
        .routes(routes!(list_cron_messages))
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
/// thousand-turn session doesn't ship in full on the initial GET.
pub const DEFAULT_HISTORY_LIMIT: usize = 50;
/// Hard cap so a misbehaving (or curious) client can't ask for the
/// whole transcript by passing `limit=999999`.
pub const MAX_HISTORY_LIMIT: usize = 200;

/// How far back to scan when fetching each session's sidebar preview.
/// A typical conversation alternates user / assistant, so the most-
/// recent user row is usually within the last couple of messages — but
/// trailing assistant tool-only turns can push it further. Ten rows
/// covers realistic shapes without paying a deep walk on long sessions.
const PREVIEW_SCAN_DEPTH: usize = 10;

/// Maximum length of the truncated preview the sidebar shows for each
/// session. Sized to fit a 260px-wide sidebar row at the web client's
/// font without wrapping; the client may truncate further with CSS.
const PREVIEW_MAX_CHARS: usize = 120;

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

// ── DTOs ─────────────────────────────────────────────────────────────

/// Response from `POST /v1/chat/sessions` and the
/// `POST /v1/chat/sessions/:id/token` token-refresh endpoint. Carries
/// the freshly-minted channel-token the web client presents on its
/// `/v1/channel-ws` upgrade via the
/// [`CHANNEL_TOKEN_HEADER`](crate::auth::CHANNEL_TOKEN_HEADER) header
/// (or the `?token=` query when the browser's WebSocket API can't set
/// custom headers — which it never can).
#[derive(Debug, Serialize, ToSchema)]
pub struct ChatSessionCredential {
    /// New (or existing) session id.
    pub session_id: String,
    /// Capability token bound to a `web/<uuid>` identity. Live for the
    /// lifetime of the session row; calling
    /// `POST /v1/chat/sessions/:id/token` revokes the previous token
    /// and returns a new one.
    pub channel_token: String,
    /// Header name the web client must use when presenting the
    /// channel-token on the channel listener.
    pub channel_token_header: &'static str,
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
    /// Absolute `session_messages.ordinal` of this row. Stable for the
    /// lifetime of the session and used both as a React key and as
    /// the `before_ordinal` cursor for the next-older page request. A
    /// `work` item carries the ordinal of the turn's first intermediate
    /// message so it sorts just after the user turn that spawned it.
    pub ordinal: i64,
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

#[derive(Debug, Serialize, ToSchema)]
pub struct ChatSessionDetail {
    pub session_id: String,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    pub hidden: bool,
    /// Active transcript slice, oldest-first within the page. The
    /// server returns at most `limit` rows; older rows are fetched
    /// by passing the lowest `ordinal` here back as the next
    /// request's `before_ordinal`.
    pub transcript: Vec<ChatTranscriptItem>,
    /// `true` when at least one older active row exists below the
    /// slice's lowest ordinal — i.e. the client should keep
    /// scroll-up pagination armed. `false` when the slice already
    /// includes the session's first message.
    pub has_more: bool,
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
    /// Preview text drawn from the session's most-recent user-authored
    /// message, truncated to [`PREVIEW_MAX_CHARS`]. The web sidebar
    /// renders this as the row label so users can scan past
    /// conversations by what they last asked. `None` for sessions
    /// without a user turn yet (a freshly-created row, or one whose
    /// transcript holds only system/tool rows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_user_text: Option<String>,
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
    /// dispatcher's fire-time framing; `aura_context::prompts::cron::original_cron_prompt`
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
/// [`aura_channels::wire::SlashCommandSpec`] so the OpenAPI surface
/// stays inside this crate's DTOs (the wire type lives in
/// `aura-channels` for sidecar reuse).
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
    responses(
        (status = 200, description = "New session id + freshly-minted channel-token", body = ChatSessionCredential),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Session creation or token mint failed", body = ErrorBody),
    )
)]
async fn create_session(State(state): State<AdminState>) -> Result<Json<ChatSessionCredential>> {
    let user = web_operator_user();
    let session = state
        .session_manager
        .create_session(user, ChannelType::http())
        .await
        .map_err(|e| GatewayError::Internal(format!("create chat session: {e}")))?;
    let session_id = session.id.clone();
    let cred = mint_credential(&state, &session_id);
    // Created emits a full patch — sibling tabs construct the row
    // straight from this without a list refetch.
    broadcast_session_patch(
        &state,
        &session_id,
        SessionPatch {
            created_at: Some(session.created_at),
            last_active: Some(session.last_active),
            hidden: Some(false),
        },
    );
    Ok(Json(cred))
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
) -> Result<Json<ChatSessionsList>> {
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
        .list_by_channel(&ChannelType::http())
        .await
        .map_err(|e| GatewayError::Internal(format!("list sessions: {e}")))?;
    let visible: Vec<Session> = scoped
        .into_iter()
        .filter(|s| query.include_hidden || !s.hidden)
        .filter(|s| query.include_cron || !is_cron_triggered(s))
        .collect();
    // Fan out the per-session preview fetch — each row needs its own
    // reverse-tail walk against `session_messages`, which is cheap
    // individually (back-of-the-index walk capped at
    // `PREVIEW_SCAN_DEPTH` rows) but adds up serially when a tab has
    // dozens of conversations open. `join_all` runs the libsql queries
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
    let items: Vec<ChatSessionSummary> = visible
        .into_iter()
        .zip(previews)
        .map(|(s, last_user_text)| ChatSessionSummary {
            session_id: s.id.to_string(),
            created_at: s.created_at,
            last_active: s.last_active,
            hidden: s.hidden,
            last_user_text,
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
) -> Result<Json<ChatSessionDetail>> {
    // `load_web_chat_session` rejects non-`http` channels with the
    // same `NotFound` shape `session_manager.get(...)` would, so a
    // caller probing a telegram/weixin id can't tell whether the
    // session exists at all — the surface stays scoped to browser-
    // originated chats just like list/token/hide/unhide above. The
    // bare `session_manager.get(...)` path that used to live here
    // would happily serve any persisted transcript.
    let (sid, session) = load_web_chat_session(&state, &session_id).await?;

    let limit = query
        .limit
        .unwrap_or(DEFAULT_HISTORY_LIMIT)
        .clamp(1, MAX_HISTORY_LIMIT);
    // Over-fetch by one row so we can answer `has_more` without an
    // extra COUNT — if the store returned `limit + 1` rows, there's at
    // least one older row beyond the window, and we drop the extra
    // before serialising.
    let mut tail = state
        .session_manager
        .history_tail(&sid, query.before_ordinal, limit + 1)
        .await
        .map_err(|e| GatewayError::Internal(format!("load history tail: {e}")))?;
    let has_more = tail.len() > limit;
    if has_more {
        // The overflow row is the *oldest* in the slice — `tail` is in
        // ascending ordinal order, so the unwanted row sits at the head
        // (it would be the start of the next-older page).
        tail.remove(0);
    }
    // Rebuild user/assistant bubbles AND the collapsed per-turn work
    // blocks (reasoning, tool calls + results, mid-turn narration) from
    // the persisted messages. Internal turns (Role::System, agent-
    // injected Role::User with `from_user=false`) are dropped; tool-use
    // iterations are folded into a `work` item rather than surfaced as
    // stray bubbles. The WS catch-up path
    // (`channel::route::chat_to_visible_wire_message`) still replays only
    // the message bubbles — a full reload through here is what restores
    // the work blocks.
    let transcript = reconstruct_transcript(tail);
    Ok(Json(ChatSessionDetail {
        session_id,
        created_at: session.created_at,
        last_active: session.last_active,
        hidden: session.hidden,
        transcript,
        has_more,
    }))
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
) -> Result<axum::http::StatusCode> {
    // Despite the `DELETE` verb this hides rather than removes the
    // row — see the module docstring. The channel-token stays alive
    // so any tab still anchored on this session keeps working; users
    // can restore via `POST .../unhide` or `?include_hidden=true`.
    let (sid, _) = load_web_chat_session(&state, &session_id).await?;
    state
        .session_manager
        .set_hidden(&sid, true)
        .await
        .map_err(|e| GatewayError::Internal(format!("hide session: {e}")))?;
    broadcast_session_patch(
        &state,
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
) -> Result<axum::http::StatusCode> {
    let (sid, session) = load_web_chat_session(&state, &session_id).await?;
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
        &sid,
        SessionPatch {
            created_at: Some(session.created_at),
            last_active: Some(session.last_active),
            hidden: Some(false),
        },
    );
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/chat/sessions/{session_id}/token",
    tag = "chat",
    params(
        ("session_id" = String, Path, description = "Session id whose token to refresh"),
    ),
    responses(
        (status = 200, description = "Fresh channel-token (old one revoked)", body = ChatSessionCredential),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Session not found", body = ErrorBody),
    )
)]
async fn refresh_session_token(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
) -> Result<Json<ChatSessionCredential>> {
    // Confirm the session exists (and is an http session) before
    // minting a token for it — minting for a non-existent session
    // would issue credentials that never reach a useful target.
    let (sid, _) = load_web_chat_session(&state, &session_id).await?;
    Ok(Json(mint_credential(&state, &sid)))
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

/// Mint a fresh web channel-token for `session_id`, store the handle
/// on [`AdminState::web_chat_tokens`] keyed by the token itself, and
/// return the credential. Caller is responsible for any "session
/// must exist" check; `create_session` issues a fresh session row
/// directly above this call so it doesn't bother.
///
/// The map is keyed by the token string so concurrent tabs anchored
/// to the same session each get their own live `TokenHandle` — keying
/// by `session_id` would make the second tab's mint drop (and
/// revoke) the first tab's handle, breaking the first tab's
/// reconnect path.
fn mint_credential(state: &AdminState, session_id: &SessionId) -> ChatSessionCredential {
    let label = format!("{WEB_CLIENT_LABEL_PREFIX}{}", uuid::Uuid::new_v4());
    let handle = state.channel_tokens.mint(ClientIdentity {
        pid: std::process::id(),
        label,
        bound_channel_type: Some(ChannelType::http().to_string()),
    });
    let token = handle.token().to_owned();
    state
        .web_chat_tokens
        .insert(token.clone(), StashedTokenHandle::new(handle));
    ChatSessionCredential {
        session_id: session_id.to_string(),
        channel_token: token,
        channel_token_header: CHANNEL_TOKEN_HEADER,
    }
}

fn web_operator_user() -> User {
    User {
        id: WEB_OPERATOR_USER_ID.to_owned(),
        name: Some("Web Operator".to_owned()),
        channel: ChannelType::http(),
    }
}

/// Load the session row for `session_id` and verify it lives on the
/// `http` channel. Both branches return the **same** `NotFound` body so
/// a request for a Telegram/WeChat session id through the chat API
/// can't be distinguished from a request for a nonexistent id —
/// `GatewayError::NotFound` serialises its `to_string()` into the JSON
/// response, so differing messages would otherwise leak existence.
async fn load_web_chat_session(
    state: &AdminState,
    session_id: &str,
) -> Result<(SessionId, Session)> {
    let sid = SessionId::from(session_id);
    let not_found = || GatewayError::NotFound(format!("chat session {session_id}"));
    let session = state
        .session_manager
        .get(&sid)
        .await
        .map_err(|e| GatewayError::Internal(format!("load session: {e}")))?
        .ok_or_else(not_found)?;
    if session.channel != ChannelType::http() {
        return Err(not_found());
    }
    Ok((sid, session))
}

/// Push a [`Frame::SessionUpdated`] patch to every connection on the
/// `http` channel — every open chat tab, whether in this browser or
/// another. The patch carries the truth (no refetch round-trip); see
/// the variant's doc comment for receiver-side merge rules. No-op
/// when the `http` channel isn't installed (only possible in test
/// fixtures that skipped `install_channels`); in that case no web
/// clients can be connected to receive it anyway.
pub(crate) fn broadcast_session_patch(
    state: &AdminState,
    session_id: &SessionId,
    patch: SessionPatch,
) {
    let Some(channel) = state.channel_registry.get(&ChannelType::http()) else {
        return;
    };
    // http is always Subscribed by construction; the `if let Some`
    // makes the typed-view constraint explicit at the call site
    // rather than reaching for an `expect` we'd never want to panic
    // on.
    if let Some(sub) = channel.as_subscribed() {
        sub.broadcast_session_patch(session_id.clone(), patch);
    }
}

/// Fetch the most-recent user-authored text for `session_id` and shape
/// it into the sidebar preview the list endpoint serves. Returns
/// `None` when the session has no user turn yet, when the user turn's
/// content is media-only, or when the underlying tail query fails —
/// the sidebar treats all three as "no preview" rather than surfacing
/// an error, so a single bad row never breaks the whole list. Walks
/// at most [`PREVIEW_SCAN_DEPTH`] rows back; deeper user turns
/// (e.g. a session whose recent activity is purely tool churn) fall
/// off the window and are accepted as missing rather than chased.
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
    manager: &aura_session::SessionManager,
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
            prompt = Some(aura_context::prompts::cron::original_cron_prompt(&text).to_owned());
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

async fn last_user_preview(
    manager: &aura_session::SessionManager,
    session_id: &SessionId,
) -> Option<String> {
    // Walk the transcript newest-first in `PREVIEW_SCAN_DEPTH`-sized
    // batches until the freshest genuine user turn turns up. Paging
    // (rather than a single capped scan) matters because a tool-heavy
    // turn appends many agent/tool rows after the user's message — a long
    // agentic loop can push it well past any fixed window, which is why a
    // tool-retry session would otherwise show "New conversation" in the
    // sidebar despite having a clear prompt. Each batch's oldest ordinal
    // seeds the next `before_ordinal`; the DB stops as soon as a user row
    // is found, so the common (recent prompt) case is still one query.
    let mut before: Option<i64> = None;
    loop {
        let batch = manager
            .history_tail(session_id, before, PREVIEW_SCAN_DEPTH)
            .await
            .ok()?;
        let oldest = batch.first().map(|(ordinal, _, _)| *ordinal);
        for (_ord, created_at, msg) in batch.iter().rev() {
            if !matches!(msg.role, Role::User) || !msg.from_user() {
                continue;
            }
            let item = message_item(0, *created_at, "user", msg)?;
            return (!item.text.is_empty()).then(|| truncate_preview(&item.text));
        }
        // No user turn in this batch — page older, or stop once the batch
        // came back short (we've reached the session's first message).
        match oldest {
            Some(ordinal) if batch.len() >= PREVIEW_SCAN_DEPTH => before = Some(ordinal),
            _ => return None,
        }
    }
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
    /// `tool_use_id` → index into `steps`, so a later tool-result message
    /// fills in the matching call's summary.
    pending_tools: HashMap<String, usize>,
}

impl WorkAccumulator {
    fn flush(&mut self, items: &mut Vec<ChatTranscriptItem>, ended_at: Option<DateTime<Utc>>) {
        if !self.steps.is_empty() {
            let started = self.started;
            items.push(ChatTranscriptItem {
                ordinal: self.ordinal.unwrap_or_default(),
                kind: TranscriptItemKind::Work,
                role: String::new(),
                text: String::new(),
                has_attachments: false,
                created_at: started.unwrap_or_else(Utc::now),
                steps: std::mem::take(&mut self.steps),
                work_started_at: started,
                work_ended_at: ended_at.or(self.last).or(started),
            });
        }
        self.ordinal = None;
        self.started = None;
        self.last = None;
        self.pending_tools.clear();
    }
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
fn reconstruct_transcript(tail: Vec<(i64, DateTime<Utc>, ChatMessage)>) -> Vec<ChatTranscriptItem> {
    let mut items: Vec<ChatTranscriptItem> = Vec::new();
    let mut work = WorkAccumulator::default();

    for (ordinal, created_at, msg) in tail {
        match msg.role {
            Role::User if msg.from_user() => {
                work.flush(&mut items, None);
                if let Some(item) = message_item(ordinal, created_at, "user", &msg) {
                    items.push(item);
                }
            }
            // Intermediate iteration: fold reasoning / narration / tool
            // calls into the open work block.
            Role::Assistant if msg.has_tool_use() => {
                if work.started.is_none() {
                    work.started = Some(created_at);
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
            // Final answer (no tool calls): close the work block, then the
            // reply bubble lands below it.
            Role::Assistant => {
                work.flush(&mut items, Some(created_at));
                if let Some(item) = message_item(ordinal, created_at, "assistant", &msg) {
                    items.push(item);
                }
            }
            Role::Tool => {
                work.last = Some(created_at);
                for block in &msg.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
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
    // that ended on a tool with no narrated reply.
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
        ordinal,
        kind: TranscriptItemKind::Message,
        role: role.to_owned(),
        text,
        has_attachments,
        created_at,
        steps: Vec::new(),
        work_started_at: None,
        work_ended_at: None,
    })
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
        let items = reconstruct_transcript(tail);
        assert_eq!(items.len(), 3);

        assert!(matches!(items[0].kind, TranscriptItemKind::Message));
        assert_eq!(items[0].role, "user");
        assert_eq!(items[0].text, "write hello");

        let work = &items[1];
        assert!(matches!(work.kind, TranscriptItemKind::Work));
        assert_eq!(
            work.ordinal, 3,
            "work block inherits first intermediate ordinal"
        );
        assert_eq!(work.work_started_at, Some(ts(3)));
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
        let items = reconstruct_transcript(tail);
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
        let items = reconstruct_transcript(tail);
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
        let items = reconstruct_transcript(tail);
        assert_eq!(items.len(), 1);
        assert!(items[0].steps[0].tool_summary.is_none());
        assert!(items[0].steps[0].tool_status.is_none());
    }

    #[test]
    fn message_item_skips_empty_keeps_text() {
        let only_tool = ChatMessage::assistant(vec![tool_use("c1", "X", serde_json::json!({}))]);
        assert!(message_item(1, ts(1), "assistant", &only_tool).is_none());

        let multi = ChatMessage::assistant(vec![text("a"), text("b")]);
        let item = message_item(1, ts(1), "assistant", &multi).expect("non-empty");
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
