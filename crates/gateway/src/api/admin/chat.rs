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

use aura_channels::wire::{Frame, SessionPatch, SlashCommandSpec};
use aura_model::{ChannelType, ChatMessage, ContentBlock, Role, Session, SessionId, User};
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
}

/// Query string for `GET /v1/chat/sessions`.
#[derive(Debug, Default, Deserialize, IntoParams)]
pub struct ListSessionsQuery {
    /// Include hidden sessions in the response. Defaults to false.
    #[serde(default)]
    pub include_hidden: bool,
}

/// Default page size for reverse transcript pagination — large enough
/// that a typical chat fits in one round-trip, small enough that a
/// thousand-turn session doesn't ship in full on the initial GET.
pub const DEFAULT_HISTORY_LIMIT: usize = 50;
/// Hard cap so a misbehaving (or curious) client can't ask for the
/// whole transcript by passing `limit=999999`.
pub const MAX_HISTORY_LIMIT: usize = 200;

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

/// One transcript row, flattened from `ChatMessage` into a shape the
/// web client can render without re-implementing the content-block
/// matcher.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChatTranscriptItem {
    /// Absolute `session_messages.ordinal` of this row. Stable for the
    /// lifetime of the session and used both as a React key and as
    /// the `before_ordinal` cursor for the next-older page request.
    pub ordinal: i64,
    /// `"user"` or `"assistant"` (or `"system"`). String rather than
    /// enum to keep the wire forgiving.
    pub role: String,
    /// Plain text content, newline-joined when multiple text blocks
    /// were present. Empty when the message was media-only.
    pub text: String,
    /// `true` when this row had non-text content (image / audio /
    /// file). The web client currently shows a placeholder.
    pub has_attachments: bool,
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
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ChatSessionsList {
    pub items: Vec<ChatSessionSummary>,
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
    // List from the session manager directly — fresh chat sessions
    // don't have any trace summary rows yet, so going through the
    // summary listing would hide them until the first agent turn
    // runs. Filtering to channel=http here keeps the list scoped to
    // browser-originated chats; admin/traces remains the cross-channel
    // surface.
    let all = state
        .session_manager
        .list()
        .await
        .map_err(|e| GatewayError::Internal(format!("list sessions: {e}")))?;
    let items: Vec<ChatSessionSummary> = all
        .into_iter()
        .filter(|s| s.channel == ChannelType::http())
        .filter(|s| query.include_hidden || !s.hidden)
        .map(|s| ChatSessionSummary {
            session_id: s.id.to_string(),
            created_at: s.created_at,
            last_active: s.last_active,
            hidden: s.hidden,
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
    let sid = SessionId::from(session_id.as_str());
    let session = state
        .session_manager
        .get(&sid)
        .await
        .map_err(|e| GatewayError::Internal(format!("load session: {e}")))?
        .ok_or_else(|| GatewayError::NotFound(format!("chat session {session_id}")))?;

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
    // `filter_map` not `map` — internal turns (Role::System, agent-
    // injected Role::User with `from_user=false`, tool-result rows,
    // empty thinking blocks) don't belong on the /chat surface.
    // Mirrors the WS catch-up replay filter in `channel::route::
    // chat_to_visible_wire_message` so a REST-loaded transcript and a
    // WS-replayed one agree on what's a user-visible bubble.
    let transcript = tail
        .into_iter()
        .filter_map(|(ordinal, msg)| chat_to_transcript_item(ordinal, msg))
        .collect();
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
        .map(SlashCommandEntry::from)
        .collect();
    Ok(Json(ListResponse::new(items)))
}

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
/// `http` channel. Both branches return `NotFound` so a request for a
/// Telegram/WeChat session id through the chat API doesn't reveal the
/// existence of non-chat sessions.
async fn load_web_chat_session(
    state: &AdminState,
    session_id: &str,
) -> Result<(SessionId, Session)> {
    let sid = SessionId::from(session_id);
    let session = state
        .session_manager
        .get(&sid)
        .await
        .map_err(|e| GatewayError::Internal(format!("load session: {e}")))?
        .ok_or_else(|| GatewayError::NotFound(format!("chat session {session_id}")))?;
    if session.channel != ChannelType::http() {
        return Err(GatewayError::NotFound(format!(
            "session {session_id} is not a web chat session"
        )));
    }
    Ok((sid, session))
}

/// Push a [`Frame::SessionUpdated`] patch to every connection on the
/// `http` channel — every open chat tab, whether in this browser or
/// another. The patch carries the truth (no refetch round-trip); see
/// the variant's doc comment for receiver-side merge rules. No-op
/// when the `http` channel isn't installed (e.g. `channels.http.
/// enabled = false`); in that case no web clients can be connected
/// to receive it anyway.
pub(crate) fn broadcast_session_patch(
    state: &AdminState,
    session_id: &SessionId,
    patch: SessionPatch,
) {
    let Some(channel) = state.channel_registry.get(&ChannelType::http()) else {
        return;
    };
    channel.broadcast_frame(Frame::SessionUpdated {
        session_id: session_id.clone(),
        patch,
    });
}

fn chat_to_transcript_item(ordinal: i64, msg: ChatMessage) -> Option<ChatTranscriptItem> {
    // Same gate as `channel::route::chat_to_visible_wire_message` —
    // see that fn for why each excluded variant doesn't belong on the
    // chat surface.
    let role = match msg.role {
        Role::User if msg.from_user => "user",
        Role::Assistant => "assistant",
        _ => return None,
    };
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
            ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. }
            | ContentBlock::Thinking { .. } => {}
        }
    }
    // A user/assistant row with neither text nor attachments is
    // structurally valid (assistant turn that produced only tool calls,
    // for instance) but would render as an empty bubble. Hide it.
    if text.is_empty() && !has_attachments {
        return None;
    }
    Some(ChatTranscriptItem {
        ordinal,
        role: role.to_owned(),
        text,
        has_attachments,
    })
}
