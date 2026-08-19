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
//! * `PUT /v1/chat/sessions/:id/archive` — archive (or unarchive) the
//!   session. Presentation only; the list endpoint keeps returning
//!   archived rows (clients group them) and new activity never clears
//!   the flag.
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
use baybo_channels::{STOP_CANCELLED_REPLY_LINE, STOP_COMMAND_NAME, StampedEvent};
use baybo_model::{
    AgentBinding, AgentFramework, ApprovalDecision, ChannelType, ChatMessage, ContentBlock,
    ControlEvent, ControlEventKind, FolderId, FolderSummary, LineageKind, LlmEntryName, Role,
    Session, SessionId, TOOL_RESULT_ERROR_PREFIX, ThinkingContent, TriggerSource, User,
};
use baybo_session::SessionError;
use baybo_store::SearchScope;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::warn;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::api::dto::{ErrorBody, ListResponse};
use crate::auth::{AuthedClient, OWNER_USER_ID};
use crate::server::AdminState;
use crate::{GatewayError, Result};

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(create_session))
        .routes(routes!(list_sessions))
        .routes(routes!(search_messages))
        .routes(routes!(get_session))
        .routes(routes!(sync_session))
        .routes(routes!(list_subagents))
        .routes(routes!(get_subagent))
        .routes(routes!(sync_subagent))
        .routes(routes!(lookup_session_message))
        .routes(routes!(set_session_model))
        .routes(routes!(set_session_pin))
        .routes(routes!(set_session_archive))
        .routes(routes!(set_session_title))
        .routes(routes!(mark_session_read))
        .routes(routes!(mark_sessions_read))
        .routes(routes!(set_session_folder))
        .routes(routes!(delete_session))
        .routes(routes!(hide_sessions))
        .routes(routes!(unhide_session))
        .routes(routes!(slash_manifest))
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
    /// Include **every** cron-triggered session, not just the ones that are
    /// conversations in their own right. Defaults to false.
    ///
    /// A recurring fire opens a real conversation (`TriggerSource::Cron
    /// { conversation: true }`) and is listed like any other. What this flag
    /// admits is the rest: one-shot fire sessions — private workspaces whose
    /// result is reported into the conversation that scheduled them — and
    /// historical fires from before that distinction existed. An operator
    /// escape hatch, not a chat-sidebar affordance.
    #[serde(default)]
    pub include_cron: bool,
}

/// Query string for `GET /v1/chat/search`.
#[derive(Debug, Deserialize, IntoParams)]
pub struct SearchMessagesQuery {
    /// What the user typed, verbatim. FTS5 syntax is NOT accepted: the store
    /// quotes the whole thing into a literal phrase, so `-`, `*`, `NEAR` and
    /// friends match themselves rather than meaning anything.
    pub q: String,
    /// Restrict to one conversation — "find it in *this* chat". Omit to search
    /// across all of them. Composes with the other filters rather than
    /// overriding them: naming a hidden session still needs `include_hidden`.
    pub session_id: Option<String>,
    /// Include sessions the user hid from their chat list. Defaults to false —
    /// on real data roughly half of a chat search's hits sit in hidden
    /// sessions, and resurfacing them is the opposite of what hiding meant.
    #[serde(default)]
    pub include_hidden: bool,
    #[serde(default)]
    pub include_archived: bool,
    /// Maximum **conversations** to return, not messages. Defaults to
    /// [`DEFAULT_SEARCH_LIMIT`], clamped to [`MAX_SEARCH_LIMIT`].
    pub limit: Option<usize>,
}

/// Enough conversations to fill a results panel without scrolling into a second
/// page.
pub const DEFAULT_SEARCH_LIMIT: usize = 20;
pub const MAX_SEARCH_LIMIT: usize = 50;

/// Raw hits pulled from the index before grouping.
///
/// Grouping has to happen over a window wider than the response, or one chatty
/// conversation buries every other. Measured on real data, `codex` matches 47
/// times across 17 conversations — but a flat top-30 covers only 7 of them,
/// because a single conversation takes 15 of the 30 slots. Client-side grouping
/// cannot fix that: the other 10 conversations were never sent.
///
/// Scanning wide is nearly free. Query cost tracks the number of MATCHES, not
/// the limit — `ORDER BY bm25` scores every hit before it drops any — so this
/// bounds the rows carried back through the trait, not the work sqlite does.
const SEARCH_SCAN_LIMIT: usize = 300;

/// Excerpts shown per conversation. Past a few, a result card stops being
/// glanceable and the conversation is better opened than previewed.
const MAX_HITS_PER_SESSION: usize = 3;

/// One matching message inside a [`ChatSearchGroup`].
#[derive(Debug, Serialize, ToSchema)]
pub struct ChatSearchHit {
    /// Position in the session's transcript. Carried so a future jump-to-message
    /// has an address; today's UI opens the session and does not use it.
    pub ordinal: i64,
    pub role: String,
    /// The ORIGINAL prose, not the segmented index text. Clients highlight by
    /// substring: a phrase of unigrams matches exactly the substring typed, so a
    /// client-side match agrees with what the index matched.
    pub text: String,
    pub created_at: DateTime<Utc>,
    /// Set when compaction stamped this row: the ordinal where that
    /// compaction's re-inserted rows begin. Every row active at that moment
    /// points at the same one, so in a compacted conversation most hits carry
    /// it.
    ///
    /// **Do not navigate here, and do not label the hit "not on screen".** The
    /// display read filters `compaction_inserted = 0`, NOT `superseded_by IS
    /// NULL` (`load_active_session_messages_tail`), so the superseded original
    /// still renders and `ordinal` is the address to jump to — while this
    /// ordinal names a re-injected machinery row that the display read excludes,
    /// so aiming a jump at it can only ever miss. What it actually reports is
    /// that the model's context was rewritten after this row: a fact about the
    /// LLM's window, not about what the user can see.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<i64>,
}

/// One conversation's matches, collapsed into a single result.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChatSearchGroup {
    pub session_id: String,
    /// Conversation title, when one has been generated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_title: Option<String>,
    /// Best-matching excerpts, best first, at most [`MAX_HITS_PER_SESSION`].
    pub hits: Vec<ChatSearchHit>,
    /// Matches this conversation has in total — `hits.len()` when it is at or
    /// under the per-conversation cap, more when it is over, so a client can say
    /// "and 12 more" without another call. Exact unless `truncated`.
    pub total_hits: usize,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ChatSearchResults {
    /// Conversations, best match first. A conversation's rank is its best hit's.
    pub groups: Vec<ChatSearchGroup>,
    /// True when the query matched more than the scan window, so some
    /// conversations are missing and `total_hits` undercounts. There is no
    /// cursor: a search box refines the query, it does not page.
    pub truncated: bool,
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
pub(crate) const UNREAD_COUNT_CAP: usize = 99;

/// How far back the second-line preview (`last_message_preview`) walks the
/// transcript tail for the newest bubble. A completed turn's final answer is
/// its LAST row (the loop ends on the first tool-free reply, and control events
/// live outside `session_messages`), so a shallow scan finds it; the extra rows
/// only tolerate an occasional non-bubble row (a compaction summary or an
/// attachment-only message) sitting above it. A turn still mid-tool-loop has no
/// final answer yet, so the scan finds no bubble and the row falls back to
/// `last_user_text` client-side — which is that turn's own prompt, exactly the
/// bubble a deeper walk would reach — so scanning deeper buys almost nothing
/// while multiplying the per-session tail fetch + deserialize.
const LAST_MESSAGE_PREVIEW_SCAN: usize = 4;

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
    /// The agent this conversation runs as: its soul, its private skills,
    /// its memory partition. Omitted or `null` ⇒ the built-in profile, which
    /// is what every channel and TUI session gets.
    ///
    /// Fixed for the session's life — there is no endpoint that changes it,
    /// because a mid-thread swap would split the memory partition and leave
    /// two personas' output in one transcript. To talk to another agent,
    /// start another conversation.
    #[serde(default)]
    pub agent_id: Option<String>,
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
/// `"reasoning"` / `"prose"` / `"tool"` / `"status"`.
#[derive(Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkStepKind {
    Reasoning,
    Prose,
    Tool,
    /// The progress observer's transient narration, reconstructed from a
    /// persisted `progress` control event — the durable shadow of the live
    /// `AgentEvent::Progress` line.
    Status,
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
    /// For a `work` item: `true` when the turn ENDED inside this reconstruction
    /// window (a real boundary — the final answer, the next user turn, or a
    /// `/stop` — closed the block); `false` when the block was cut off by the
    /// page window's edge and the turn continues into the adjacent (older) page.
    /// The client fuses a cut-off head (`false`) with the following half so a
    /// turn split across a page boundary stays one card, and NEVER fuses a
    /// complete block (`true`) with its neighbour — that neighbour is a
    /// different turn (e.g. a completed turn whose empty final reply produced no
    /// bubble, abutting a following cron fire). `None` for `message` / `notice`
    /// items, which never participate in the fold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_complete: Option<bool>,
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
    /// Playback length in ms for `audio` (see `WireAttachment::duration_ms`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u32>,
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
            duration_ms: att.duration_ms,
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
    /// Compaction boundaries for this session — same value as
    /// [`ChatSessionDetail::compaction_points`], carried on the sync
    /// response so a client that loads its transcript through the sync
    /// loop (the iOS bundle) gets the pre-compaction divider without a
    /// separate meta fetch. Present on EVERY sync (baseline and
    /// difference), not just the baseline: a client that persists its
    /// cursor re-opens with a difference sync, so gating this to the
    /// baseline would strand the divider on every warm re-entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compaction_points: Vec<CompactionPoint>,
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
    /// The call's id, set when `kind == Tool` — the SAME id the live
    /// `ToolStarted` / `ToolCompleted` frames carry.
    ///
    /// This surface withheld it once, on the reasoning that only the live
    /// client needs it (to pair a later `ToolCompleted`). But it is also the
    /// step's IDENTITY, and a client folds reconstructed steps routinely — a
    /// turn longer than one page reconstructs per-page, and the halves join
    /// client-side. Without it every reconstructed call in a block looks alike,
    /// so folding two halves silently drops all but the first, while folding a
    /// live block with its own reconstruction double-renders every call.
    /// `None` only for a call whose row predates this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
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
    /// Decision the call's approval prompt returned (`"approve"` /
    /// `"approve_always"` / `"deny"`), read from the persisted
    /// `ToolResultMeta`; `None` when the call never prompted (or the row
    /// predates the field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<String>,
    /// When this step happened — the `created_at` of the row it came from, or
    /// the instant the live buffer recorded it. Lets a client time the
    /// stretches BETWEEN the model's mid-turn remarks rather than only the
    /// turn as a whole. `None` for a row reconstructed by a gateway that
    /// predates this, and for the synthetic steps that have no source row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
}

impl ChatWorkStep {
    /// Stamp this step with when it happened. Chained at construction so each
    /// call site names its own source of truth.
    #[must_use]
    fn stamped(mut self, at: DateTime<Utc>) -> Self {
        self.at = Some(at);
        self
    }

    fn reasoning(text: String) -> Self {
        Self {
            kind: WorkStepKind::Reasoning,
            text,
            call_id: None,
            tool: None,
            tool_label: None,
            tool_status: None,
            tool_summary: None,
            approval: None,
            at: None,
        }
    }

    fn prose(text: String) -> Self {
        Self {
            kind: WorkStepKind::Prose,
            text,
            call_id: None,
            tool: None,
            tool_label: None,
            tool_status: None,
            tool_summary: None,
            approval: None,
            at: None,
        }
    }

    fn status(text: String) -> Self {
        Self {
            kind: WorkStepKind::Status,
            text,
            call_id: None,
            tool: None,
            tool_label: None,
            tool_status: None,
            tool_summary: None,
            approval: None,
            at: None,
        }
    }

    fn tool(call_id: String, tool: String, tool_label: Option<String>) -> Self {
        Self {
            kind: WorkStepKind::Tool,
            text: String::new(),
            call_id: Some(call_id),
            tool: Some(tool),
            tool_label,
            tool_status: None,
            tool_summary: None,
            approval: None,
            at: None,
        }
    }
}

/// Project the shared wire fold onto the REST shape. `call_id` carries across —
/// it is the step's identity, not merely the live client's pairing key (see
/// [`ChatWorkStep::call_id`]); `status` / `summary` map straight onto
/// `tool_status` / `tool_summary` (both `None` while a tool is still running).
impl From<WireWorkStep> for ChatWorkStep {
    fn from(step: WireWorkStep) -> Self {
        let at = step.at;
        let mut out = match step.kind {
            WireWorkStepKind::Reasoning => Self::reasoning(step.text),
            WireWorkStepKind::Prose => Self::prose(step.text),
            WireWorkStepKind::Status => Self::status(step.text),
            WireWorkStepKind::Tool => Self {
                kind: WorkStepKind::Tool,
                text: String::new(),
                call_id: step.call_id,
                tool: step.tool,
                tool_label: step.label,
                tool_status: step.status,
                tool_summary: step.summary,
                approval: step.approval.map(|d| d.as_str().to_owned()),
                at: None,
            },
        };
        out.at = at;
        out
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
    /// Per-session model pick within `last_llm`'s entry
    /// (`session.state.last_model`), or `null` for the entry's default.
    /// Drives which model row the header picker checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_model: Option<String>,
    /// Per-session reasoning-effort pick (`session.state.last_effort`), or
    /// `null` for the entry default. Drives the header's thinking-level check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_effort: Option<String>,
    /// Auto-generated conversation title, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Compaction boundaries for this session — one entry per context
    /// compaction, ascending by ordinal, empty when the session has never
    /// been compacted. Each marks where a compaction rewrote the LLM
    /// context; the transcript itself still shows the real pre-compaction
    /// messages (their superseded originals), and the web draws a
    /// "pre-compaction history" divider before the first displayed row at
    /// or after each `ordinal`. Session-level metadata, independent of the
    /// returned page slice, so it is stable across scroll-up pagination and
    /// carried only on the baseline/meta fetch (`before_ordinal` absent).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compaction_points: Vec<CompactionPoint>,
}

/// One context-compaction boundary: the summary head compaction wrote at
/// `ordinal`, stamped with the compaction time (`at`, the head row's
/// `created_at`). The transcript still renders the real messages below it;
/// this only tells the client where to draw the boundary divider.
#[derive(Debug, Serialize, ToSchema)]
pub struct CompactionPoint {
    pub ordinal: i64,
    pub at: DateTime<Utc>,
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
    /// True when the user has archived this session. Always emitted —
    /// the list never filters on it, so clients with an archived view
    /// group rows themselves and clients without one keep showing every
    /// row; set via `PUT /v1/chat/sessions/{id}/archive`.
    pub archived: bool,
    /// Preview text drawn from the session's most-recent user-authored
    /// message, truncated to [`PREVIEW_MAX_CHARS`]. The web sidebar
    /// renders this as the row label so users can scan past
    /// conversations by what they last asked. `None` for sessions
    /// without a user turn yet (a freshly-created row, or one whose
    /// transcript holds only system/tool rows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_user_text: Option<String>,
    /// Preview text drawn from the session's most-recent **displayable**
    /// message regardless of author — the newest user prompt or final
    /// assistant answer carrying text, truncated to [`PREVIEW_MAX_CHARS`].
    /// Telegram-style list clients render this as the row's second line so
    /// the preview follows the conversation (an agent reply shows once it
    /// lands), while [`Self::last_user_text`] stays the user-only label the
    /// web sidebar uses. `None` when the scanned tail holds only tool /
    /// media rows or the session has no turn yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message_text: Option<String>,
    /// The user-created folder this session is filed under, or absent for
    /// uncategorized. Set via `PUT /v1/chat/sessions/{id}/folder`; the web
    /// sidebar groups rows by this id.
    ///
    /// **Ignored for a cron conversation** — those group by [`Self::cron_job_id`]
    /// instead (see `docs/cron-groups.md`), so a fire can never be in a cron
    /// group and a user folder at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<String>,
    /// The cron job whose fire this conversation is, for the clients that
    /// collapse a turn's fires into one chat-list row (a **cron group** — a
    /// derived view, never a `session_folders` row; see `docs/cron-groups.md`).
    /// `None` for a user session, and for the one-shot fire workspaces the list
    /// never returns anyway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron_job_id: Option<String>,
    /// The label for that group: the turn's **live** title while the turn exists
    /// (so a rename propagates with no rewrite of any session), falling back to
    /// the title snapshotted onto the fire at mint once the turn is deleted.
    /// `None` only when both are unavailable — a pre-snapshot fire whose turn is
    /// gone; clients leave those rows flat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron_job_title: Option<String>,
    /// Whether this row's **cron group** is pinned to the top of the chat list.
    /// Read off the live turn (`cron_jobs.pinned`), so every fire of the turn
    /// carries the same value and the client folds it into the one group row —
    /// exactly as it already does for `cron_job_title`. The group is a view, so
    /// the bit necessarily dies with the turn: a tombstone group (turn deleted,
    /// history kept) is always unpinned. `false` for a non-cron row.
    #[serde(default)]
    pub cron_group_pinned: bool,
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
    /// True while a tool call in this session is parked on the approval gate,
    /// waiting for the user to approve or deny it. The client marks the row so
    /// the user knows which conversation to open; the prompt itself is only
    /// answerable inside the conversation.
    ///
    /// Derived from live in-memory gate state, never the store: a parked turn
    /// dies with the gateway process, so reading `false` after a restart is
    /// correct rather than stale. Absent when false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub approval_pending: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ChatSessionsList {
    pub items: Vec<ChatSessionSummary>,
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
    let session = create_or_load_chat_session(
        &state,
        requested.session_id,
        requested.agent_id,
        user,
        channel_type.clone(),
    )
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
            archived: Some(session.archived),
            // A freshly-created session is always uncategorized; absent =
            // no change, which a newly-constructed client row renders as
            // uncategorized.
            folder_id: None,
            title: None,
            // A session that has never run a turn cannot be parked on the
            // approval gate; the queue publishes its own edges from there on.
            approval_pending: None,
        },
    );
    Ok(Json(ChatSessionCreated {
        session_id: session_id.to_string(),
    }))
}

#[utoipa::path(
    get,
    path = "/chat/search",
    tag = "chat",
    params(SearchMessagesQuery),
    responses(
        (status = 200, description = "Matching messages, best first", body = ChatSearchResults),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Search failed", body = ErrorBody),
    )
)]
async fn search_messages(
    State(state): State<AdminState>,
    Query(query): Query<SearchMessagesQuery>,
    authed: Option<Extension<AuthedClient>>,
) -> Result<Json<ChatSearchResults>> {
    // Scoped to the caller's chat channel like every other chat route, which
    // also keeps a subagent's own run (channel `subagent`) out of a chat search
    // — its session is not one this UI can open. See `docs/search.md`.
    let channel = chat_list_channel(authed.as_ref().map(|ext| &ext.0));
    let scope = SearchScope {
        channel: Some(channel),
        session: query.session_id.clone().map(SessionId::from),
        include_hidden: query.include_hidden,
        include_archived: query.include_archived,
        // Policy, not a knob: this route answers "find a conversation I can
        // open", and a cron workspace is precisely the session that cannot be
        // opened — `/v1/chat/sessions` drops it and the attach path 404s it. No
        // query param exposes this; an operator view that wants fire transcripts
        // is a different product with a different scope, and turning it on is a
        // query-side change with no reindex.
        include_cron_workspaces: false,
    };
    let limit = query
        .limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .min(MAX_SEARCH_LIMIT);
    let hits = state
        .message_search
        .search_messages(&query.q, &scope, SEARCH_SCAN_LIMIT as u32)
        .await
        .map_err(|e| GatewayError::Internal(format!("search messages: {e}")))?;
    let truncated = hits.len() == SEARCH_SCAN_LIMIT;

    // Hits arrive sorted by bm25, so first-seen order IS relevance order: a
    // conversation ranks by its best hit, which falls out of pushing each hit
    // onto its group in arrival order. Nothing re-sorts.
    let mut order: Vec<String> = Vec::new();
    let mut grouped: std::collections::HashMap<String, ChatSearchGroup> =
        std::collections::HashMap::new();
    for hit in hits {
        let sid = hit.session_id.as_str().to_owned();
        let group = grouped.entry(sid.clone()).or_insert_with(|| {
            order.push(sid.clone());
            ChatSearchGroup {
                session_id: sid,
                session_title: None,
                hits: Vec::new(),
                total_hits: 0,
            }
        });
        group.total_hits += 1;
        // Count every match but carry only the best few: `total_hits` is what
        // lets a client say "and 12 more" without a second call.
        if group.hits.len() == MAX_HITS_PER_SESSION {
            continue;
        }
        let text = hit
            .message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        group.hits.push(ChatSearchHit {
            ordinal: hit.ordinal,
            role: hit.message.role.as_str().to_owned(),
            text,
            created_at: hit.created_at,
            superseded_by: hit.superseded_by,
        });
    }

    // Titles are fetched AFTER the cut, so the scan window's width costs no
    // extra lookups — and in one grouped flat-column query instead of a
    // point lookup per result conversation. A title batch this build cannot
    // load is a missing subtitle, not a failed search: degrade to empty
    // rather than 500 the whole result set.
    let cut: Vec<String> = order.into_iter().take(limit).collect();
    let title_ids: Vec<SessionId> = cut
        .iter()
        .map(|sid| SessionId::from(sid.as_str()))
        .collect();
    let mut titles = state
        .session_manager
        .session_titles(&title_ids)
        .await
        .unwrap_or_default();
    let mut groups = Vec::with_capacity(cut.len());
    for sid in cut {
        let Some(mut group) = grouped.remove(&sid) else {
            continue;
        };
        group.session_title = titles.remove(&SessionId::from(sid.as_str())).flatten();
        groups.push(group);
    }
    Ok(Json(ChatSearchResults { groups, truncated }))
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
    // Web and device both operate on the single `owner` chat channel, so this
    // is `owner` for every chat caller. Push the filter into SQL so a
    // long-running gateway with thousands of bot sessions (telegram / weixin /
    // …) doesn't pay an O(all-sessions) sqlite round-trip on every chat-list
    // refresh. We still walk the result to apply the hidden filter; that's a
    // userland-only pass over the (already scoped) result.
    //
    // Going through `session_manager` here rather than the trace
    // summary listing is deliberate: fresh chat sessions don't have
    // any trace summary rows yet, so the summary path would hide
    // them until the first agent turn ran.
    let channel_type = chat_list_channel(authed.as_ref().map(|ext| &ext.0));
    let scoped = state
        .session_manager
        .list_by_channel(&channel_type)
        .await
        .map_err(|e| GatewayError::Internal(format!("list sessions: {e}")))?;
    let visible: Vec<Session> = scoped
        .into_iter()
        .filter(|s| query.include_hidden || !s.hidden)
        .filter(|s| !s.trigger.is_project_session())
        .filter(|s| query.include_cron || !is_private_cron_session(s))
        .collect();
    // One grouped scan for every per-session aggregate the sidebar
    // needs (first-line previews, second-line tail windows, unread
    // counts) — three store queries for the whole list instead of a
    // per-session round-trip fan-out. A failed scan degrades to empty
    // maps (no previews, zero badges) rather than failing the list;
    // the next refresh retries.
    let ids: Vec<SessionId> = visible.iter().map(|s| s.id.clone()).collect();
    let scan = state
        .session_manager
        .chat_list_scan(&ids, LAST_MESSAGE_PREVIEW_SCAN, UNREAD_COUNT_CAP)
        .await
        .unwrap_or_default();
    let cron_jobs = live_cron_job_meta(&state, &visible).await;
    // One queue pass for the whole list. Taken here, with no `.await` between
    // the snapshot and its use below, so every row reads one consistent
    // instant of the gate's state.
    let waiting_on_approval = state
        .channel_registry
        .get(&channel_type)
        .map(|ch| ch.pending_approval_sessions())
        .unwrap_or_default();
    let items: Vec<ChatSessionSummary> = visible
        .into_iter()
        .map(|s| {
            let last_user_text = scan
                .last_user
                .get(&s.id)
                .and_then(|(created_at, msg)| last_user_preview(*created_at, msg));
            let last_message_text = scan
                .tails
                .get(&s.id)
                .and_then(|tail| last_message_preview(tail));
            let unread = scan.unread_counts.get(&s.id).copied().unwrap_or(0);
            ChatSessionSummary {
                cron_job_title: cron_group_label(&s, &cron_jobs),
                cron_job_id: cron_group_id(&s).map(str::to_owned),
                cron_group_pinned: cron_group_pinned(&s, &cron_jobs),
                session_id: s.id.to_string(),
                created_at: s.created_at,
                last_active: s.last_active,
                hidden: s.hidden,
                pinned: s.pinned,
                archived: s.archived,
                last_user_text,
                last_message_text,
                folder_id: s.folder_id.as_ref().map(|f| f.to_string()),
                unread_count: unread as i64,
                approval_pending: waiting_on_approval.contains(&s.id),
                title: s.title.clone(),
            }
        })
        .collect();
    Ok(Json(ChatSessionsList { items }))
}

/// The cron job this row groups under, or `None` for anything that is not a
/// listed cron fire. Keyed off [`TriggerSource::is_cron_conversation`], not the
/// bare trigger kind: a one-shot's private workspace is not a conversation and
/// must never surface a group (it is not in the list at all — this keeps the two
/// facts from drifting apart).
fn cron_group_id(session: &Session) -> Option<&str> {
    session
        .trigger
        .is_cron_conversation()
        .then(|| session.trigger.cron_job_id())
        .flatten()
}

/// What a live cron job contributes to its group's chat-list row.
struct CronGroupMeta {
    /// The turn's current title. `None` when it has none (a pre-title row) — the
    /// group then falls back to the fire's snapshot. NOT a reason to drop the
    /// turn from the map: it still carries `pinned`.
    title: Option<String>,
    /// Whether the user pinned this turn's group (`cron_jobs.pinned`).
    pinned: bool,
}

/// Every cron job any listed fire belongs to. One batched read (cron jobs number
/// in the handful), skipped entirely when the page holds no cron conversation. A
/// failed read degrades to an empty map rather than failing the list — every
/// group then falls back to its tombstone name and reads as unpinned.
///
/// An untitled turn stays IN the map with `title: None`. It used to be filtered
/// out wholesale, which was harmless while the title was all this carried — but
/// it also carries the pin now, and dropping the row would silently unpin the
/// group of any job that has no title.
async fn live_cron_job_meta(
    state: &AdminState,
    visible: &[Session],
) -> HashMap<String, CronGroupMeta> {
    if !visible.iter().any(|s| cron_group_id(s).is_some()) {
        return HashMap::new();
    }
    match state.cron_scheduler.list_all_jobs().await {
        Ok(jobs) => jobs
            .into_iter()
            .map(|job| {
                let meta = CronGroupMeta {
                    title: (!job.title.is_empty()).then_some(job.title),
                    pinned: job.pinned,
                };
                (job.id, meta)
            })
            .collect(),
        Err(e) => {
            warn!(error = %e, "cron jobs unavailable; groups fall back to their snapshots");
            HashMap::new()
        }
    }
}

/// Whether this row's cron group is pinned. Read off the LIVE job — the group is
/// a view, so the job is the only thing that can hold the bit, and it therefore
/// dies with the job: deleting a job releases its group (a tombstone group,
/// which outlives its job via the fire's title snapshot, is always unpinned).
fn cron_group_pinned(session: &Session, live: &HashMap<String, CronGroupMeta>) -> bool {
    cron_group_id(session)
        .and_then(|turn_id| live.get(turn_id))
        .is_some_and(|meta| meta.pinned)
}

/// The label for this row's cron group: the turn's **live** title, so a rename
/// propagates to every client on the next list fetch without rewriting a single
/// session; falling back to the title snapshotted onto the fire at mint, which
/// answers the one question the live lookup cannot — *the turn is gone; what was
/// this history called?*
///
/// `None` when both are unavailable (a fire minted before the snapshot existed,
/// whose turn has since been deleted). Clients leave such a row flat rather than
/// inventing a name — the population is self-limiting.
fn cron_group_label(session: &Session, live: &HashMap<String, CronGroupMeta>) -> Option<String> {
    let turn_id = cron_group_id(session)?;
    live.get(turn_id)
        .and_then(|meta| meta.title.clone())
        .or_else(|| session.trigger.cron_job_title().map(str::to_owned))
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
    session_detail(
        &state,
        sid,
        session,
        session_id,
        query.before_ordinal,
        query.limit,
    )
    .await
}

/// The read half of `GET /chat/sessions/{id}`, `GET /chat/subagents/{id}` and
/// the board's `GET /projects/{id}/issues/{n}/runs/{attempt}/transcript`.
/// Only ADMISSION differs between those routes; what they return must not, so
/// the page is built once here rather than transcribed per route.
pub(crate) async fn session_detail(
    state: &AdminState,
    sid: SessionId,
    session: Session,
    session_id: String,
    before_ordinal: Option<i64>,
    limit: Option<usize>,
) -> Result<Json<ChatSessionDetail>> {
    let limit = limit
        .unwrap_or(DEFAULT_HISTORY_LIMIT)
        .clamp(1, MAX_HISTORY_LIMIT);
    let page = build_history_page(state, &sid, &session, before_ordinal, limit).await?;
    // Session-level divider metadata, consumed only off the baseline/meta
    // fetch (`before_ordinal` absent) — the limit-1 open where the client
    // decides where to draw its pre-compaction dividers. Scroll-up backfill
    // pages discard it, so skip the lookup there rather than recomputing it
    // per page. Best-effort — a lookup failure just omits the dividers
    // rather than failing the load.
    let compaction_points = if before_ordinal.is_none() {
        state
            .session_manager
            .compaction_boundaries(&sid)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(session_id = %sid, error = %e, "chat: compaction boundaries lookup failed");
                Vec::new()
            })
            .into_iter()
            .map(|(ordinal, at)| CompactionPoint { ordinal, at })
            .collect()
    } else {
        Vec::new()
    };
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
        last_model: session.state.last_model.clone(),
        last_effort: session.state.last_effort.clone(),
        title: session.title.clone(),
        compaction_points,
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
            match state
                .session_manager
                .list_control_events_in_range(sid, lower, last)
                .await
            {
                Ok(events) => events,
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
            .turn_lifecycle
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
        &compaction_watermarks(state, sid).await,
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
    session_sync(&state, sid, session, query).await
}

/// The read half of `GET …/sync` for both route families — see
/// [`session_detail`] for why the body is shared rather than transcribed.
async fn session_sync(
    state: &AdminState,
    sid: SessionId,
    session: Session,
    query: SyncSessionQuery,
) -> Result<Json<ChatSyncResponse>> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_HISTORY_LIMIT)
        .clamp(1, MAX_HISTORY_LIMIT);

    let rebased = if let Some(since) = query.since_ordinal {
        match sync_difference(state, &sid, since, limit).await? {
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

    let page = build_history_page(state, &sid, &session, None, limit).await?;
    Ok(Json(ChatSyncResponse {
        rows: page.transcript,
        // The tail scan starts at the newest persisted row, so the
        // page's newest ordinal IS the coverage watermark; `None` iff
        // the session has no rows.
        next_cursor: page.newest_ordinal,
        rebased,
        oldest_ordinal: page.oldest_ordinal,
        has_more_older: page.has_more,
        compaction_points: sync_compaction_points(state, &sid).await,
    }))
}

/// Compaction boundaries as `CompactionPoint`s for the sync response —
/// best-effort (a lookup failure omits the divider rather than failing the
/// sync). Carried on EVERY sync so a cursor-persisting client (iOS) gets the
/// pre-compaction divider on warm re-entry too, not only its first baseline.
async fn sync_compaction_points(state: &AdminState, sid: &SessionId) -> Vec<CompactionPoint> {
    state
        .session_manager
        .compaction_boundaries(sid)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(session_id = %sid, error = %e, "chat: sync compaction boundaries lookup failed");
            Vec::new()
        })
        .into_iter()
        .map(|(ordinal, at)| CompactionPoint { ordinal, at })
        .collect()
}

/// Compaction watermark ordinals (summary-head ordinals) for this session,
/// best-effort — `reconstruct_transcript` breaks a work block across each so a
/// mid-turn compaction's pre-/post halves never fold into one card that would
/// swallow the divider. A lookup failure just leaves the block unsplit.
async fn compaction_watermarks(state: &AdminState, sid: &SessionId) -> Vec<i64> {
    state
        .session_manager
        .compaction_boundaries(sid)
        .await
        .map(|b| b.into_iter().map(|(ordinal, _)| ordinal).collect())
        .unwrap_or_default()
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
        match state
            .session_manager
            .list_control_events_in_range(sid, since, upper)
            .await
        {
            Ok(events) => events,
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
    // `SubscribeState` bundle's turn, NOT sync's. But we DO align the trailing
    // in-flight turn's reconstructed work block to the active turn's
    // `started_at` (one indexed turn read): a mid-turn difference reconstructs
    // that turn's persisted tool rows into a partial `work` block, and without
    // the alignment its `w<ordinal>`/start wouldn't match the live
    // (SubscribeState-opened) block — the client would render TWO blocks for
    // one turn. Aligning the start lets the client reconcile them into one.
    let active_turn_started = state
        .turn_lifecycle
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
        &compaction_watermarks(state, sid).await,
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
        compaction_points: sync_compaction_points(state, sid).await,
    }))
}

/// Page size for a subagent listing. The fan-out limiter bounds CONCURRENT
/// breadth, not the cumulative count — an overnight agentic conversation can
/// leave hundreds of children behind — and this surface polls while it is on
/// screen, so the page is a real cursor rather than a truncation.
const SUBAGENT_PAGE: usize = 50;

/// How a child's work ended, as far as its turn rows can say.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChatSubagentStatus {
    /// Spawned, but its actor has not opened a turn yet.
    Pending,
    /// At least one turn is still open.
    Running,
    Completed,
    Failed,
    Cancelled,
    /// Terminal (its turns carry an end) under a `status_kind` this gateway
    /// does not know — schema drift, reported rather than guessed at.
    Unknown,
}

/// One direct subagent child of a session.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChatSubagentSummary {
    pub session_id: String,
    /// Profile the parent spawned (`explorer`, `general-purpose`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
    /// `"baybo"`, or the external agent's name (`"claude"` / `"codex"`).
    pub backend: String,
    /// The errand the parent authored, stamped onto the child's title at
    /// spawn. `None` for children spawned before that stamp existed — the
    /// client falls back to the profile name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    pub status: ChatSubagentStatus,
    pub created_at: DateTime<Utc>,
    /// When its first turn began. `None` until one opens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    /// When its last turn ended; `None` while anything is still open. Sent as
    /// a pair with `started_at` rather than a precomputed duration so a client
    /// can tick a running child's clock without polling for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
}

/// Response from `GET /v1/chat/sessions/{session_id}/subagents`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChatSubagentList {
    /// Ascending by `created_at` — the transcript's own direction, so the
    /// newest (usually the running one) is last.
    pub items: Vec<ChatSubagentSummary>,
    /// Older children exist below `items[0]`. The client pages back by sending
    /// that row's `created_at` + `session_id` as the cursor.
    pub has_more_older: bool,
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct SubagentListQuery {
    /// Keyset cursor: return children strictly OLDER than this
    /// `(created_at, session_id)` pair. Both or neither — a timestamp alone
    /// cannot separate the siblings one turn's fan-out mints in the same
    /// microsecond.
    #[serde(default)]
    pub before_created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub before_id: Option<String>,
}

#[utoipa::path(
    get,
    path = "/chat/sessions/{session_id}/subagents",
    tag = "chat",
    params(
        ("session_id" = String, Path, description = "Parent session id. Either an owner-chat conversation or a readable subagent child (drilling from a child into its own children)."),
        SubagentListQuery,
    ),
    responses(
        (status = 200, description = "Direct subagent children, ascending", body = ChatSubagentList),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Session not found", body = ErrorBody),
    )
)]
async fn list_subagents(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
    Query(query): Query<SubagentListQuery>,
    authed: Option<Extension<AuthedClient>>,
) -> Result<Json<ChatSubagentList>> {
    let authed = authed.as_ref().map(|ext| &ext.0);
    let (sid, _parent) = load_readable_parent_session(&state, &session_id, authed).await?;

    // Both halves of the cursor or neither: a partial one would silently page
    // from a different place than the client meant.
    let before = match (query.before_created_at, query.before_id.as_deref()) {
        (Some(at), Some(id)) => Some((at, SessionId::from(id))),
        _ => None,
    };
    // Over-fetch by one to answer `has_more_older` without a COUNT, the same
    // trick `build_history_page` uses.
    let mut children = state
        .session_manager
        .store()
        .list_lineage_children_page(&sid, before, SUBAGENT_PAGE + 1)
        .await
        .map_err(|e| GatewayError::Internal(format!("list subagent children: {e}")))?;
    let has_more_older = children.len() > SUBAGENT_PAGE;
    children.truncate(SUBAGENT_PAGE);
    // The store pages NEWEST first (that is the direction a cursor walks); the
    // sheet reads oldest-first like the transcript it belongs to.
    children.reverse();

    // One grouped query for every child's liveness and wall clock. The sheet
    // polls while it is open, so a per-child turn read is not an option.
    let bounds: HashMap<SessionId, baybo_store::SessionTurnBounds> = state
        .turn_lifecycle
        .session_turn_bounds(&children.iter().map(|s| s.id.clone()).collect::<Vec<_>>())
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "chat: subagent turn bounds failed");
            Vec::new()
        })
        .into_iter()
        .map(|b| (b.session_id.clone(), b))
        .collect();

    let items = children
        .into_iter()
        .map(|child| {
            let bound = bounds.get(&child.id);
            ChatSubagentSummary {
                status: subagent_status(bound),
                started_at: bound.map(|b| b.first_started_at),
                ended_at: bound.and_then(|b| b.last_ended_at),
                session_id: child.id.as_ref().to_string(),
                subagent_type: child.state.subagent_type.clone(),
                backend: subagent_backend_name(&child),
                task: child.title.clone(),
                created_at: child.created_at,
            }
        })
        .collect();

    Ok(Json(ChatSubagentList {
        items,
        has_more_older,
    }))
}

/// `"baybo"` unless the child was tagged with an external backend at genesis.
/// A missing tag reads as `baybo`: that is what a pre-tag child ran.
fn subagent_backend_name(child: &Session) -> String {
    match &child.state.subagent_backend {
        Some(baybo_model::SubagentBackendTag::External { external_kind, .. }) => {
            external_kind.as_str().to_owned()
        }
        _ => baybo_model::BAYBO_BACKEND_TAG.to_owned(),
    }
}

fn subagent_status(bounds: Option<&baybo_store::SessionTurnBounds>) -> ChatSubagentStatus {
    let Some(bounds) = bounds else {
        return ChatSubagentStatus::Pending;
    };
    if bounds.live {
        return ChatSubagentStatus::Running;
    }
    match bounds.latest_status_kind.as_str() {
        "completed" => ChatSubagentStatus::Completed,
        "failed" => ChatSubagentStatus::Failed,
        "cancelled" => ChatSubagentStatus::Cancelled,
        _ => ChatSubagentStatus::Unknown,
    }
}

#[utoipa::path(
    get,
    path = "/chat/subagents/{session_id}",
    tag = "chat",
    params(
        ("session_id" = String, Path, description = "Subagent child session id"),
        GetSessionQuery,
    ),
    responses(
        (status = 200, description = "Child detail + transcript slice", body = ChatSessionDetail),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Session not found", body = ErrorBody),
    )
)]
async fn get_subagent(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
    Query(query): Query<GetSessionQuery>,
    authed: Option<Extension<AuthedClient>>,
) -> Result<Json<ChatSessionDetail>> {
    let authed = authed.as_ref().map(|ext| &ext.0);
    let (sid, session) = load_readable_subagent_session(&state, &session_id, authed).await?;
    session_detail(
        &state,
        sid,
        session,
        session_id,
        query.before_ordinal,
        query.limit,
    )
    .await
}

#[utoipa::path(
    get,
    path = "/chat/subagents/{session_id}/sync",
    tag = "chat",
    params(
        ("session_id" = String, Path, description = "Subagent child session id"),
        SyncSessionQuery,
    ),
    responses(
        (status = 200, description = "Forward-recovery pull for a child transcript", body = ChatSyncResponse),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Session not found", body = ErrorBody),
    )
)]
async fn sync_subagent(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
    Query(query): Query<SyncSessionQuery>,
    authed: Option<Extension<AuthedClient>>,
) -> Result<Json<ChatSyncResponse>> {
    let authed = authed.as_ref().map(|ext| &ext.0);
    let (sid, session) = load_readable_subagent_session(&state, &session_id, authed).await?;
    session_sync(&state, sid, session, query).await
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
    /// The model to pick WITHIN `llm`'s entry — one of that entry's
    /// `[model] + model_candidates`. `null`/absent uses the entry's default
    /// model. Ignored (and rejected as a mismatch) when `llm` is `null`,
    /// since there is no entry to pick a model within.
    #[serde(default)]
    pub model: Option<String>,
    /// Per-session reasoning effort
    /// (`none`/`minimal`/`low`/`medium`/`high`/`xhigh`), or `null`/absent for
    /// the entry's default. Applies to every turn of THIS session only (not a
    /// global entry edit); consumed by providers that support it
    /// (openai-subscription), clamped per model at runtime.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SetSessionModelResponse {
    /// The pin now in effect: the entry name, or `null` for `default-llm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_llm: Option<String>,
    /// The model pick now in effect within the entry, or `null` for the
    /// entry's default model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_model: Option<String>,
    /// The reasoning-effort pick now in effect, or `null` for the entry
    /// default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_effort: Option<String>,
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

    // A model pick only means something within an entry. Reject
    // `{llm: null, model: "x"}` rather than silently dropping it; clear the
    // model when no entry is pinned. Otherwise validate the model belongs to
    // the entry's `[model] + model_candidates` (rejects a stranded pick up
    // front instead of letting it degrade to the entry default at run time).
    let model_pick: Option<String> = match (&pin, req.model.as_deref()) {
        (_, None) => None,
        (None, Some(_)) => {
            return Err(GatewayError::BadRequest(
                "model pick requires an llm entry; send llm together with model".to_string(),
            ));
        }
        (Some(entry), Some(model)) => {
            super::validate_llm_model(&state, entry, model)?;
            Some(model.to_string())
        }
    };

    // Reasoning effort is a free per-session knob (the runtime clamps it per
    // model), but reject a value outside the known ladder so a typo surfaces
    // as a 400 rather than silently degrading to the default every turn.
    let effort_pick: Option<String> = match req.reasoning_effort.as_deref().map(str::trim) {
        None | Some("") => None,
        // Canonicalised on the way in, so `none` and `off` do not persist as
        // two spellings of one rung.
        Some(level) => match baybo_llm::effort::ReasoningEffort::parse(level) {
            Some(rung) => Some(rung.as_str().to_string()),
            None => {
                return Err(GatewayError::BadRequest(format!(
                    "unknown reasoning_effort {level:?}; expected one of {}",
                    effort_ladder()
                )));
            }
        },
    };

    // Persist the pin durably FIRST, via targeted flat-column writes
    // (`set_last_llm` / `set_last_model`). Unlike a full-session `save`,
    // these can't be clobbered by a concurrent `touch` (load + full blob
    // save fired on every inbound message) — the same flat-column discipline
    // the `hidden` flag uses. Synchronous, so a storage failure surfaces as
    // an error here instead of a false 200, and authoritative for any actor
    // spawned later (the spawner reads `session.state.last_llm` /
    // `last_model`, which `get` patches from these columns).
    state
        .session_manager
        .set_last_llm(&sid, pin.as_ref())
        .await
        .map_err(|e| GatewayError::Internal(format!("persist session model pin: {e}")))?;
    state
        .session_manager
        .set_last_model(&sid, model_pick.as_deref())
        .await
        .map_err(|e| GatewayError::Internal(format!("persist session model pick: {e}")))?;
    state
        .session_manager
        .set_last_effort(&sid, effort_pick.as_deref())
        .await
        .map_err(|e| GatewayError::Internal(format!("persist session effort pick: {e}")))?;

    // Then re-pin any *live* actor in memory so the switch takes effect
    // on its next turn without waiting for eviction + rehydration.
    // Unconditional: a `false` return just means no actor is live right
    // now, in which case the persisted pin above already covers the next
    // spawn. (A spawn racing in the µs window between this persist and
    // the route can still start on the prior pin for its lifetime; the
    // store stays correct, so it self-heals on the next eviction.)
    let applied_to_live_actor = state
        .supervisor
        .route(
            &sid,
            AgentMessage::SetModel {
                llm: pin.clone(),
                model: model_pick.clone(),
                effort: effort_pick.clone(),
            },
        )
        .await;

    Ok(Json(SetSessionModelResponse {
        last_llm: pin.map(|n| n.to_string()),
        last_model: model_pick,
        last_effort: effort_pick,
        applied_to_live_actor,
    }))
}

/// The rungs the per-session pin accepts, for the 400's message. The ladder
/// itself lives in `baybo_llm::effort` — mirroring it here is how the gateway
/// ended up two rungs behind it.
fn effort_ladder() -> String {
    baybo_llm::effort::ReasoningEffort::ALL
        .iter()
        .map(|l| l.as_str())
        .collect::<Vec<_>>()
        .join(", ")
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

/// Request body for `PUT /v1/chat/sessions/{session_id}/archive`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetSessionArchiveRequest {
    /// `true` to move this session into the archived group, `false` to
    /// restore it to the main chat list.
    pub archived: bool,
}

/// Request body for `PUT /v1/chat/sessions/{session_id}/title`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetSessionTitleRequest {
    /// The conversation's new title. Interior whitespace is collapsed and the
    /// ends trimmed; the result must be non-empty and at most
    /// `baybo_model::MAX_SESSION_TITLE_LEN` characters, or the call is a 400.
    ///
    /// There is no "clear it and let the model re-title" form: a cleared title
    /// cannot be expressed on the wire, where an absent `SessionPatch.title`
    /// already means "unchanged".
    pub title: String,
}

/// Map a session-argument failure onto its status. Kept apart from
/// [`folder_err`] so a bad title reports as a title problem rather than
/// borrowing the folder wording.
fn session_title_err(e: SessionError) -> GatewayError {
    match e {
        SessionError::NotFound(m) => GatewayError::NotFound(m),
        SessionError::InvalidArgument(m) => GatewayError::BadRequest(m),
        other => GatewayError::Internal(format!("set session title: {other}")),
    }
}

#[utoipa::path(
    put,
    path = "/chat/sessions/{session_id}/title",
    tag = "chat",
    params(
        ("session_id" = String, Path, description = "Session id to rename"),
    ),
    request_body = SetSessionTitleRequest,
    responses(
        (status = 204, description = "Title updated"),
        (status = 400, description = "Empty or over-long title", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Session not found", body = ErrorBody),
    )
)]
async fn set_session_title(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
    authed: Option<Extension<AuthedClient>>,
    Json(req): Json<SetSessionTitleRequest>,
) -> Result<axum::http::StatusCode> {
    let authed = authed.as_ref().map(|ext| &ext.0);
    let (sid, session) = load_scoped_chat_session(&state, &session_id, authed).await?;
    // Targeted flat-column write — like `set_pinned`, it survives a
    // concurrent `touch` (full-blob save) so the rename can't be clobbered.
    let stored = state
        .session_manager
        .set_user_title(&sid, req.title)
        .await
        .map_err(session_title_err)?;
    // Broadcast the STORED title, never the submitted one: the manager
    // normalizes, and shipping the raw value would leave every other client
    // showing something the next list refetch silently rewrites.
    broadcast_session_patch(
        &state,
        &session.channel,
        &sid,
        SessionPatch {
            title: Some(stored),
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
    path = "/chat/sessions/{session_id}/archive",
    tag = "chat",
    params(
        ("session_id" = String, Path, description = "Session id to archive or unarchive"),
    ),
    request_body = SetSessionArchiveRequest,
    responses(
        (status = 204, description = "Archive state updated"),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Session not found", body = ErrorBody),
    )
)]
async fn set_session_archive(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
    authed: Option<Extension<AuthedClient>>,
    Json(req): Json<SetSessionArchiveRequest>,
) -> Result<axum::http::StatusCode> {
    let authed = authed.as_ref().map(|ext| &ext.0);
    let (sid, session) = load_scoped_chat_session(&state, &session_id, authed).await?;
    // Targeted flat-column write — like `set_pinned`, it survives a
    // concurrent `touch` (full-blob save) so the flag can't be clobbered.
    state
        .session_manager
        .set_archived(&sid, req.archived)
        .await
        .map_err(|e| GatewayError::Internal(format!("set session archive: {e}")))?;
    // Broadcast so every open chat client moves the row between the main
    // list and the archived group without a list refetch.
    broadcast_session_patch(
        &state,
        &session.channel,
        &sid,
        SessionPatch {
            archived: Some(req.archived),
            ..SessionPatch::default()
        },
    );
    Ok(axum::http::StatusCode::NO_CONTENT)
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

/// Request body for `POST /v1/chat/sessions/read`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct MarkManyReadRequest {
    /// The sessions to mark fully read. Each cursor is advanced to that
    /// session's newest ordinal, so — unlike the per-session route — the caller
    /// needs no ordinal of its own. Sessions it cannot see are rejected; ids
    /// that do not exist are skipped.
    pub session_ids: Vec<String>,
}

/// Cap on one batch. A cron group is the motivating caller and a `*/30` job
/// accrues 48 fires a day, so the bound is generous — but it is a bound: an
/// unbounded list would be an unbounded fan-out of store round-trips behind one
/// request.
const MAX_MARK_READ_BATCH: usize = 500;

#[utoipa::path(
    post,
    path = "/chat/sessions/read",
    tag = "chat",
    request_body = MarkManyReadRequest,
    responses(
        (status = 204, description = "Every named session's read cursor advanced to its newest ordinal"),
        (status = 400, description = "Batch too large", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "A named session is not visible to this client", body = ErrorBody),
    )
)]
async fn mark_sessions_read(
    State(state): State<AdminState>,
    authed: Option<Extension<AuthedClient>>,
    Json(req): Json<MarkManyReadRequest>,
) -> Result<axum::http::StatusCode> {
    if req.session_ids.len() > MAX_MARK_READ_BATCH {
        return Err(GatewayError::BadRequest(format!(
            "at most {MAX_MARK_READ_BATCH} sessions per batch, got {}",
            req.session_ids.len()
        )));
    }
    let authed = authed.as_ref().map(|ext| &ext.0);
    // Scope-check every id BEFORE writing anything: a batch that would touch a
    // session this client cannot see is refused whole, not half-applied.
    let sids = check_scoped_chat_sessions(&state, &req.session_ids, authed).await?;
    // "Fully read" is the session's own tail, resolved server-side — a chat-list
    // client has no ordinals. A session with no rows yet has no tail and needs
    // no cursor. The write is the same max-wins flat-column update the
    // per-session route uses, so a racing single mark cannot regress it.
    futures::future::join_all(sids.into_iter().map(|sid| {
        let manager = state.session_manager.clone();
        async move {
            let Ok(Some(tail)) = manager.latest_session_ordinal(&sid).await else {
                return;
            };
            if let Err(e) = manager.set_read_cursor(&sid, tail).await {
                warn!(session_id = %sid, error = %e, "mark-read skipped one session in a batch");
            }
        }
    }))
    .await;
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

/// Request body for `POST /v1/chat/sessions/hide`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct HideManyRequest {
    /// The sessions to hide. Like the per-session `DELETE`, this preserves every
    /// row — see the module docstring. Sessions the caller cannot see are
    /// rejected.
    pub session_ids: Vec<String>,
}

/// Cap on one batch, matching [`MAX_MARK_READ_BATCH`] — the motivating caller is
/// the same cron group, whose fires this hides in one gesture.
const MAX_HIDE_BATCH: usize = 500;

#[utoipa::path(
    post,
    path = "/chat/sessions/hide",
    tag = "chat",
    request_body = HideManyRequest,
    responses(
        (status = 204, description = "Every named session hidden (rows preserved on the server)"),
        (status = 400, description = "Batch too large", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "A named session is not visible to this client", body = ErrorBody),
    )
)]
async fn hide_sessions(
    State(state): State<AdminState>,
    authed: Option<Extension<AuthedClient>>,
    Json(req): Json<HideManyRequest>,
) -> Result<axum::http::StatusCode> {
    if req.session_ids.len() > MAX_HIDE_BATCH {
        return Err(GatewayError::BadRequest(format!(
            "at most {MAX_HIDE_BATCH} sessions per batch, got {}",
            req.session_ids.len()
        )));
    }
    let authed = authed.as_ref().map(|ext| &ext.0);
    // Scope-check every id BEFORE writing anything: a batch that would touch a
    // session this client cannot see is refused whole, not half-applied. Every
    // in-scope session sits on the caller's chat channel by definition.
    let targets = check_scoped_chat_sessions(&state, &req.session_ids, authed).await?;
    let channel = chat_list_channel(authed);
    // Sequential, and the first failure propagates — unlike the mark-read batch,
    // which warns and skips. A read cursor converges on the next list pull; a
    // hide is a user-visible mutation the client has already applied
    // optimistically, so it must learn that the batch did not fully land.
    for sid in targets {
        state
            .session_manager
            .set_hidden(&sid, true)
            .await
            .map_err(|e| GatewayError::Internal(format!("hide session: {e}")))?;
        broadcast_session_patch(
            &state,
            &channel,
            &sid,
            SessionPatch {
                hidden: Some(true),
                ..SessionPatch::default()
            },
        );
    }
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
            // Carry the live pin + archive state so a sibling tab
            // re-adding the row drops it straight into the correct block.
            pinned: Some(session.pinned),
            archived: Some(session.archived),
            // Carry the folder assignment too so the re-added row lands in
            // the right folder (absent ⇒ uncategorized).
            folder_id: session.folder_id.as_ref().map(|f| FolderChange::Set {
                id: f.as_str().to_owned(),
            }),
            title: session.title.clone(),
            // Absent = no change. A prompt parked while the row was hidden
            // keeps its mark from the queue's own edge; the re-added row picks
            // it up on the next list merge.
            approval_pending: None,
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
    // Folders are owner-wide, not session-scoped: one broadcast on the shared
    // owner channel reaches every synced surface (web + device).
    if let Some(channel) = state.channel_registry.get(&ChannelType::owner())
        && let Some(sub) = channel.as_subscribed()
    {
        sub.broadcast_folders_changed(folders);
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
    requested_agent_id: Option<String>,
    user: User,
    channel_type: ChannelType,
) -> Result<Session> {
    let Some(session_id) = requested_session_id else {
        let binding = resolve_agent_binding(state, requested_agent_id.as_deref()).await?;
        return state
            .session_manager
            .create_session_with_agent(user, channel_type, binding)
            .await
            .map_err(|e| GatewayError::Internal(format!("create chat session: {e}")));
    };

    // Look the session up *before* validating the requested agent. The binding
    // is immutable and outlives its profile row by design, so an idempotent
    // retry carrying the original `agent_id` must still return the session even
    // if that profile has since been deleted or moved to an external
    // framework — validation belongs to the create half only.
    let sid = SessionId::from(session_id.as_str());
    if let Some(existing) = state
        .session_manager
        .get(&sid)
        .await
        .map_err(|e| GatewayError::Internal(format!("load requested chat session: {e}")))?
    {
        // Scope by channel only (`owner` for every chat caller). The `user.id`
        // equality the pre-unification code also checked is intentionally
        // gone: one gateway is one owner, and pre-unification rows still carry
        // the old `web-operator`/`device_id` ids in `user.id` (only the
        // `channel` is migrated), so equating it would 404 legacy sessions.
        if existing.channel != channel_type || is_excluded_from_global_chat(&existing) {
            return Err(GatewayError::NotFound(format!("chat session {session_id}")));
        }
        return Ok(existing);
    }

    let binding = resolve_agent_binding(state, requested_agent_id.as_deref()).await?;
    state
        .session_manager
        .get_or_create_with_agent(&sid, user, channel_type, binding)
        .await
        .map_err(|e| GatewayError::Internal(format!("create requested chat session: {e}")))
}

/// Validate a requested `agent_id` and materialise that agent's persona
/// directory, so the first turn's soul assembly finds a `SOUL.md` rather than
/// racing to create one.
///
/// `None` (built-in) binds nothing: an unbound session resolves to
/// `personas/baybo/` plus the workspace `skills/`, which already exist.
async fn resolve_agent_binding(
    state: &AdminState,
    requested: Option<&str>,
) -> Result<Option<AgentBinding>> {
    let Some(requested) = requested.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let agent_id = crate::api::admin::agents::parse_agent_id(requested)?;
    let row = state
        .agent_profile_store
        .get(&agent_id)
        .await
        .map_err(|e| GatewayError::Internal(format!("load agent profile: {e}")))?
        .ok_or_else(|| GatewayError::BadRequest(format!("unknown agent profile {requested}")))?;
    if row.framework != AgentFramework::Baybo {
        // Serving a top-level chat through an external CLI is a separate
        // leg (turn dispatch, resume keys, working-dir materialisation); a
        // clear refusal beats binding a session nothing can run.
        return Err(GatewayError::BadRequest(format!(
            "agent {} runs on {}, which cannot host a chat session yet",
            row.id,
            row.framework.as_str()
        )));
    }
    if !agent_id.is_builtin() {
        baybo_workspace::ensure_persona_layout(
            &state.workspace_paths,
            agent_id.as_str(),
            baybo_workspace::prompt::PERSONA_SOUL_TEMPLATE,
        )
        .await
        .map_err(|e| GatewayError::Internal(format!("materialise agent persona: {e}")))?;
    }
    Ok(Some(AgentBinding {
        agent_id,
        framework: row.framework,
    }))
}

/// The channel every pooled chat surface operates on: the single shared
/// `owner` pool. Web and device authenticate distinctly (and each registers
/// under its own type on the wire, for leaked-token containment), but their
/// sessions, memory, and cost all live under one owner identity — so past the
/// auth boundary the chat REST layer no longer distinguishes them, and new
/// rows carry `owner`, not a per-surface tag. Shared with the cron routes,
/// whose turns are scoped the same way. (`_authed` is retained for call-site
/// symmetry; the surface no longer changes the channel.)
pub(crate) fn chat_list_channel(_authed: Option<&AuthedClient>) -> ChannelType {
    ChannelType::owner()
}

/// The identity every chat session is stamped with. One owner across all
/// synced surfaces (web + device) — shared memory namespace and cost ledger
/// (see [`OWNER_USER_ID`]). The originating surface (`http`/`device`) is kept
/// only as `channel` provenance, never as the identity, so a conversation
/// started on the phone and continued on the web is one owner's thread.
fn chat_user(authed: Option<&AuthedClient>) -> User {
    User {
        id: OWNER_USER_ID.to_owned(),
        name: Some("Owner".to_owned()),
        channel: chat_list_channel(authed),
    }
}

/// Load the session row for `session_id` and verify it lives on the chat
/// channel (`owner` — the shared web+device pool). A session on `tui` or a
/// `Multiplexed` (Telegram/WeChat) channel returns the **same** `NotFound`
/// body as a nonexistent id — `GatewayError::NotFound` serialises its
/// `to_string()` into the JSON response, so differing messages would leak
/// existence.
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

/// Bound on the lineage walk in [`load_readable_subagent_session`]. Mirrors
/// `spawn_subagent`'s own depth-check cap and exists for the same reason: a
/// corrupt chain must terminate the walk rather than spin. Real chains are
/// three deep (`DEFAULT_MAX_SUBAGENT_DEPTH`).
const MAX_LINEAGE_WALK_HOPS: u32 = 128;

/// Load a subagent child session and admit it only if a client could already
/// open the conversation it ultimately descends from.
///
/// Walking the lineage chain — rather than reading the denormalised
/// `root_session_id` in one hop — is deliberate. That column is written at
/// genesis and read by no query anywhere (`idx_sessions_root` was dropped in
/// the 2026-07 audit for exactly that reason), so promoting it to a permission
/// decision would make a single bad write a silent authorization bug with
/// nothing to catch it. `parent_session_id` is indexed and is the same chain
/// `spawn_subagent`'s depth cap already trusts.
///
/// The root must ALSO not be a hidden cron fire. Production fire sessions are
/// minted on the cron job's own channel, so a job scheduled from an owner
/// conversation yields an owner-channel fire session — and a one-shot fire is
/// a private workspace the chat list drops and the attach path 404s. Testing
/// only `channel == owner` would open a side door into a conversation no
/// client is meant to reach. The rule is "the root is a session you could
/// already open", not "the root is on the owner channel".
///
/// Every rejection returns the SAME `NotFound` body as an unknown id, for the
/// reason [`load_scoped_chat_session`] documents.
async fn load_readable_subagent_session(
    state: &AdminState,
    session_id: &str,
    authed: Option<&AuthedClient>,
) -> Result<(SessionId, Session)> {
    let sid = SessionId::from(session_id);
    let not_found = || GatewayError::NotFound(format!("subagent session {session_id}"));
    let load = |id: SessionId| async move {
        state
            .session_manager
            .get(&id)
            .await
            .map_err(|e| GatewayError::Internal(format!("load session: {e}")))
    };

    let child = load(sid.clone()).await?.ok_or_else(not_found)?;
    // A session with no lineage is not a subagent, whatever else it is. This
    // route must never become a second way to read an ordinary conversation.
    match child.lineage.as_ref().map(|l| &l.kind) {
        Some(LineageKind::Subagent) => {}
        None => return Err(not_found()),
    }

    let mut root = child.clone();
    for _ in 0..MAX_LINEAGE_WALK_HOPS {
        let Some(lineage) = root.lineage.clone() else {
            break;
        };
        root = load(lineage.parent_session_id)
            .await?
            .ok_or_else(not_found)?;
    }
    // Still parented after the cap ⇒ a cycle or a chain far beyond anything
    // `spawn_subagent` can produce. Refuse rather than admit on a walk that
    // never reached a root.
    if root.lineage.is_some() {
        return Err(not_found());
    }
    if root.channel != chat_list_channel(authed) || is_private_cron_session(&root) {
        return Err(not_found());
    }
    Ok((sid, child))
}

/// Resolve a session that may be either an ordinary owner-chat conversation or
/// a readable subagent child. Only the child LISTING needs this: drilling from
/// a child into its own children asks the same question of an id that is not
/// on the owner channel.
/// A hidden one-shot cron fire is refused HERE as well, even though
/// `load_scoped_chat_session` admits it: that route already serves the fire by
/// id, but nothing ever hands a client one, and enumerating its children would
/// be exposure with no legitimate caller. Falling through to the subagent path
/// makes the refusal automatic — a fire session has no lineage, so it fails
/// there — and returns the identical `NotFound` body.
async fn load_readable_parent_session(
    state: &AdminState,
    session_id: &str,
    authed: Option<&AuthedClient>,
) -> Result<(SessionId, Session)> {
    match load_scoped_chat_session(state, session_id, authed).await {
        Ok((sid, session)) if !is_private_cron_session(&session) => Ok((sid, session)),
        _ => load_readable_subagent_session(state, session_id, authed).await,
    }
}

/// Batch form of [`load_scoped_chat_session`]'s scope check: one grouped
/// flat-column query instead of a session load per id. Refuses the whole
/// batch when any id is unknown or off the caller's chat channel —
/// callers write nothing on error, so a bad batch is rejected whole, not
/// half-applied.
async fn check_scoped_chat_sessions(
    state: &AdminState,
    session_ids: &[String],
    authed: Option<&AuthedClient>,
) -> Result<Vec<SessionId>> {
    let sids: Vec<SessionId> = session_ids
        .iter()
        .map(|s| SessionId::from(s.as_str()))
        .collect();
    let channels = state
        .session_manager
        .session_channels(&sids)
        .await
        .map_err(|e| GatewayError::Internal(format!("load sessions: {e}")))?;
    let want = chat_list_channel(authed);
    for sid in &sids {
        if channels.get(sid).map(String::as_str) != Some(want.as_str()) {
            return Err(GatewayError::NotFound(format!("chat session {sid}")));
        }
    }
    Ok(sids)
}

/// Push a [`Frame::SessionUpdated`] patch to every open chat client on
/// `channel` — the `owner` pool, so a web tab and a phone on the session both
/// receive it. The patch carries the truth (no refetch round-trip); see the
/// variant's doc comment for receiver-side merge rules. No-op when the channel
/// is not installed (only possible in test fixtures that skipped
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

/// Nudge every client on `channel` that its session list is stale (the
/// session-less `Frame::Gap`). For changes with no session row to patch — see
/// [`baybo_channels::SubscribedView::broadcast_list_stale`], whose motivating
/// case is the cron-group pin.
pub(crate) fn broadcast_session_list_stale(state: &AdminState, channel: &ChannelType) {
    let Some(channel) = state.channel_registry.get(channel) else {
        return;
    };
    if let Some(sub) = channel.as_subscribed() {
        sub.broadcast_list_stale();
    }
}

/// Whether a cron fire owns no conversation the user can read or continue.
///
/// This covers one-shot private workspaces and historical fires from before
/// recurring fires became conversations. A recurring fire's session *is* its
/// notification, so it is listed and replyable like any other conversation.
pub(crate) fn is_private_cron_session(session: &Session) -> bool {
    matches!(session.trigger, TriggerSource::Cron { .. }) && !session.trigger.is_cron_conversation()
}

/// Whether a session belongs outside the global chat surface.
///
/// Private cron workspaces have no conversation of their own. Board
/// conversations instead belong to the board: an issue session is reached
/// through its card and transcript, never through the global chat list.
pub(crate) fn is_excluded_from_global_chat(session: &Session) -> bool {
    session.trigger.is_project_session() || is_private_cron_session(session)
}

async fn transcript_attachments(
    rows: &[(i64, DateTime<Utc>, ChatMessage)],
    blob_store: &dyn baybo_store::BlobStore,
) -> HashMap<i64, Vec<ChatAttachment>> {
    // Concurrent per-row resolution: each attachment-carrying row costs
    // a blob `stat` lookup, and an image-heavy page paid them serially.
    let resolved = futures::future::join_all(
        rows.iter()
            .filter(|(_, _, msg)| {
                msg.content.iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::Image { .. }
                            | ContentBlock::Audio { .. }
                            | ContentBlock::File { .. }
                    )
                })
            })
            .map(|(ordinal, _created_at, msg)| async move {
                let (_text, wire_attachments) =
                    crate::channel::adapter::split_content(&msg.content, blob_store).await;
                (*ordinal, wire_attachments)
            }),
    )
    .await;
    resolved
        .into_iter()
        .filter(|(_, wire)| !wire.is_empty())
        .map(|(ordinal, wire)| {
            (
                ordinal,
                wire.into_iter().map(ChatAttachment::from).collect(),
            )
        })
        .collect()
}

/// Shape a session's most-recent user-authored message (from the
/// grouped chat-list scan) into the sidebar's first-line preview.
/// `None` when the turn is media-only — the sidebar renders "no
/// preview". `message_item` extracts the display text the same way
/// the transcript does; the ordinal it stamps is unused here.
fn last_user_preview(
    created_at: chrono::DateTime<chrono::Utc>,
    msg: &baybo_model::ChatMessage,
) -> Option<String> {
    let item = message_item(0, created_at, "user", msg, Vec::new())?;
    (!item.text.is_empty()).then(|| truncate_preview(&item.text))
}

/// Newest **displayable** message regardless of author — the freshest user
/// prompt or final assistant answer carrying text — collapsed into the chat
/// list's second-line preview. Walks the tail window (from the grouped
/// chat-list scan) newest-first and returns the first row that renders as a
/// message BUBBLE, applying the same visibility rules as
/// [`reconstruct_transcript`] so the preview never surfaces
/// something the transcript itself hides: only a real user turn
/// ([`ChatMessage::from_user`]) or a tool-free final assistant answer counts —
/// agent-injected user rows (cron / recalled-memory framing), work-block
/// narration (an assistant row with tool calls), and tool rows are skipped, and
/// the model-facing cancelled-turn marker is stripped. `None` when the scanned
/// tail holds no such bubble (media-only, a mid-tool-loop turn, or a fresh
/// session) — the row then falls back to its title / user preview client-side.
/// The window is bounded to [`LAST_MESSAGE_PREVIEW_SCAN`] rows so a long
/// tool loop can't turn one preview into an unbounded tail read.
/// The user-bubble predicate every chat surface shares: a genuine channel
/// input ([`baybo_model::ChatMessage::from_user`]), or a spawned child's
/// opening errand — the ONE agent-injected row that should render (see
/// [`baybo_model::MessageSource::SubagentSeed`]; rows from before that
/// variant existed are plain `agent` and stay hidden, deliberately —
/// provenance, never content-sniffing, is what separates the errand from the
/// skill-reminder machinery that shares its shape).
fn renders_as_user_bubble(msg: &baybo_model::ChatMessage) -> bool {
    msg.from_user()
        || msg.source() == baybo_model::MessageSource::SubagentSeed
        // A board run's brief is the ask its whole transcript answers, and the
        // ONLY transcripts holding one are issue runs' — so this needs no
        // per-reader knob. It used to be one, and a shared read path carrying
        // a parameter for a single caller is a parameter that can be passed
        // wrong; the row's own provenance cannot be.
        || msg.source() == baybo_model::MessageSource::IssueBrief
}

fn last_message_preview(
    tail: &[(i64, chrono::DateTime<chrono::Utc>, baybo_model::ChatMessage)],
) -> Option<String> {
    tail.iter().rev().find_map(|(_ordinal, created_at, msg)| {
        // Match reconstruct_transcript's bubble rules: a real user turn, or a
        // tool-free final assistant answer. Everything else (agent-injected
        // user rows, assistant work-block narration, tool results) renders no
        // bubble there, so it must not become a preview here.
        let role = if renders_as_user_bubble(msg) {
            "user"
        } else if matches!(msg.role, Role::Assistant) && !msg.has_tool_use() {
            "assistant"
        } else {
            return None;
        };
        let item = message_item(0, *created_at, role, msg, Vec::new())?;
        // Strip the model-facing cancelled-turn marker (a no-op when absent):
        // a /stop-salvaged reply keeps only its partial text, and a
        // thinking-only marker-only row strips to empty and is skipped — the
        // same "frame for the model, strip for the user" contract the
        // transcript honours.
        let text = baybo_context::prompts::cancelled_turn::strip_marker(&item.text);
        (!text.is_empty()).then(|| truncate_preview(text))
    })
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
    /// `complete` is `true` when a real turn boundary (final answer, next user
    /// turn, `/stop`) closes the block, `false` when the page window's trailing
    /// edge cut it off mid-turn — the client fuses only a cut-off block with the
    /// half in the adjacent page.
    fn flush(
        &mut self,
        items: &mut Vec<ChatTranscriptItem>,
        ended_at: Option<DateTime<Utc>>,
        complete: bool,
    ) {
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
                turn_complete: Some(complete),
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
fn in_flight_work_steps(events: Vec<StampedEvent>) -> Vec<ChatWorkStep> {
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
        &[],
    )
}

fn reconstruct_transcript_with_attachments(
    tail: Vec<(i64, DateTime<Utc>, ChatMessage)>,
    control_events: Vec<ControlEvent>,
    active_turn_started: Option<DateTime<Utc>>,
    in_flight_steps: Vec<ChatWorkStep>,
    attachments_by_ordinal: &HashMap<i64, Vec<ChatAttachment>>,
    // Compaction watermarks (summary-head ordinals) whose machinery is hidden
    // from this display. A work block must never fold ACROSS one: a mid-turn
    // compaction leaves the pre- and post-compaction halves of a turn adjacent
    // (the machinery between them is elided), so without a forced break here
    // they'd render as one card that swallows the pre-compaction divider.
    compaction_watermarks: &[i64],
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
    // When the newest turn this page closed produced its answer. Read by the
    // trailing in-flight fold to tell a still-running turn's buffered work from
    // a finished turn's leftovers — see the trailing fold's per-step filter.
    let mut last_answer_at: Option<DateTime<Utc>> = None;

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
                // Progress narration is NOT a turn boundary: it folds INTO the
                // open work block as a `status` step (the durable shadow of the
                // live `notice { transient }` line) instead of flushing the
                // block and emitting its own row. Seed the accumulator if this
                // is the turn's first step (progress fired before any tool
                // iteration persisted), inheriting the turn start so timing is
                // consistent with the tool path.
                if ev.kind == ControlEventKind::Progress {
                    if work.started.is_none() {
                        work.started = Some(turn_started.unwrap_or(ev.created_at));
                        work.ordinal = Some(ev.after_ordinal);
                    }
                    work.last = Some(ev.created_at);
                    work.steps
                        .push(ChatWorkStep::status(ev.text).stamped(ev.created_at));
                    continue;
                }
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
                work.flush(&mut items, Some(ended_at), true);
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
        // Close the open work block at a compaction watermark it straddles —
        // `complete = true` so the client keeps it distinct from the
        // post-compaction half (never fuses across the seam). The next row then
        // opens a fresh block, and the pre-compaction divider lands between the
        // two. Reset `turn_started` so the continuation times from its own
        // resume, not across the elided machinery.
        if let Some(block_start) = work.ordinal
            && compaction_watermarks
                .iter()
                .any(|&w| block_start < w && w <= ordinal)
        {
            work.flush(&mut items, None, true);
            turn_started = None;
        }
        match msg.role {
            Role::User if renders_as_user_bubble(&msg) => {
                work.flush(&mut items, None, true);
                turn_started = Some(created_at);
                if let Some(mut item) = message_item(
                    ordinal,
                    created_at,
                    "user",
                    &msg,
                    attachments_by_ordinal
                        .get(&ordinal)
                        .cloned()
                        .unwrap_or_default(),
                ) {
                    // The board shows a brief to a PERSON, and the framing
                    // around it is written for the model — who it is, that
                    // nobody is waiting at a keyboard, where its checkout is.
                    // Rendered whole it buries the one line the reader opened
                    // the panel for. Same shape as the cancelled-turn marker
                    // below: strip what the prompt module added, on the way
                    // out, at the one surface that shows it.
                    if msg.source() == baybo_model::MessageSource::IssueBrief {
                        item.text = baybo_context::prompts::issue::unframe_issue_brief(&item.text)
                            .to_string();
                    }
                    if item.text.is_empty() && !item.has_attachments {
                        continue;
                    }
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
                                work.steps
                                    .push(ChatWorkStep::reasoning(text).stamped(created_at));
                            }
                        }
                        ContentBlock::Text(t) if !t.trim().is_empty() => {
                            work.steps
                                .push(ChatWorkStep::prose(t.clone()).stamped(created_at));
                        }
                        ContentBlock::ToolUse {
                            id, name, input, ..
                        } => {
                            work.pending_tools.insert(id.clone(), work.steps.len());
                            work.steps.push(
                                ChatWorkStep::tool(
                                    id.clone(),
                                    name.clone(),
                                    tool_label(name, input),
                                )
                                .stamped(created_at),
                            );
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
                            work.steps
                                .push(ChatWorkStep::reasoning(text).stamped(created_at));
                        }
                    }
                }
                let cancelled = next_is_cancelling_stop[idx];
                if cancelled {
                    work.cancelled = true;
                }
                work.flush(&mut items, Some(created_at), true);
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
                last_answer_at = Some(created_at);
            }
            Role::Tool => {
                work.last = Some(created_at);
                for block in &msg.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        meta,
                    } = block
                        && let Some(&idx) = work.pending_tools.get(tool_use_id)
                        && let Some(step) = work.steps.get_mut(idx)
                    {
                        let approval = meta.as_ref().and_then(|m| m.approval);
                        // A recorded `Deny` outranks the text sniff: a tool that
                        // prompted MID-CALL folds the refusal into its own error
                        // message, which carries none of the sentinel wording, so
                        // sniffing alone would reconstruct it as a plain failure
                        // while the live view showed it denied. Rows persisted
                        // before the field existed still fall through to the sniff.
                        step.tool_status = Some(match approval {
                            Some(ApprovalDecision::Deny) => "denied".to_owned(),
                            _ => tool_result_status(content),
                        });
                        step.tool_summary = Some(summarize_tool_result(content));
                        step.approval = approval.map(|d| d.as_str().to_owned());
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
    //
    // Only the steps that are still THIS turn's, though. The buffer is cleared by
    // the turn's own `Message` / `TurnState{inactive}` fan-out, which runs after
    // the answer row is persisted — so through that finalization window a page
    // holds both the finished turn's reconstructed card and a full replay of the
    // same work, and folding it emits the turn a second time below its own reply,
    // seeded (`last_ordinal`) with that very reply's ordinal: the
    // `[work][reply][work]` duplicate. The cut is per STEP, on the step's own
    // instant, rather than a gate on the turn latch: latch and buffer are read
    // from two different places at two different moments, so the NEXT turn's
    // start can already be visible while the previous turn's steps are still
    // buffered, and a whole-buffer gate would wave the duplicate through on the
    // strength of a turn the steps don't belong to. Every buffered step is
    // stamped when the channel records it (`StampedEvent`), so an unstamped one
    // is a synthetic this can't place — drop it rather than guess it is new.
    let in_flight_steps: Vec<ChatWorkStep> = match last_answer_at {
        Some(answered) => in_flight_steps
            .into_iter()
            .filter(|s| s.at.is_some_and(|at| at > answered))
            .collect(),
        None => in_flight_steps,
    };
    // The turn still running BELOW that answer, if any. An `active_turn_started`
    // at or before it is the latch of the turn the page already shows finished:
    // `active_turn_started_at` lingers through post-answer finalization.
    let live_turn_started =
        active_turn_started.filter(|start| last_answer_at.is_none_or(|answered| *start > answered));
    let has_in_flight = !in_flight_steps.is_empty();
    if has_in_flight {
        // A `status` step reaches the trailing block from TWO sources at once for
        // the in-flight turn: the persisted `progress` control events (folded
        // above, at their anchor positions) AND the live channel's in-flight
        // buffer. Drop the buffered duplicates so the same narration line isn't
        // rendered twice — the positioned control-event copy wins; any status
        // line the buffer holds that hasn't persisted yet still appends.
        let folded_status: std::collections::HashSet<String> = work
            .steps
            .iter()
            .filter(|s| s.kind == WorkStepKind::Status)
            .map(|s| s.text.clone())
            .collect();
        work.steps.extend(
            in_flight_steps
                .into_iter()
                .filter(|s| s.kind != WorkStepKind::Status || !folded_status.contains(&s.text)),
        );
        if work.ordinal.is_none() {
            work.ordinal = Some(last_ordinal);
        }
    }
    // When a turn is still in flight, align this trailing block's start with
    // the live `TurnState`'s `started_at` (the turn start instant) rather than
    // the first message's timestamp. Both are computed from
    // `active_turn_started_at`, so they match exactly — which is what lets a
    // reloading tab *reopen* this block on the next `turn_state{active}`
    // (`workStartedAt === startedAt`) instead of opening a second one. Only the
    // turn running BELOW the newest answer qualifies: a lingering latch belongs
    // to the turn whose answer is already above, and stamping it on a block down
    // here would hand the client a reopen key for the wrong turn.
    if let Some(start) = live_turn_started
        && !work.steps.is_empty()
    {
        work.started = Some(start);
    }
    // Trailing flush: the page window's edge closed the block, not a real turn
    // boundary — so the turn continues in the adjacent (older) page. Mark it
    // cut-off so the client fuses it with that page's other half.
    work.flush(&mut items, None, false);
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
        turn_complete: None,
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
        id: baybo_model::control_event_row_id(ev.seq),
        ordinal: None,
        kind,
        role,
        text: ev.text,
        has_attachments: false,
        // A `Command` echo carries the send's id so a client's optimistic
        // command bubble reconciles with this durable row instead of a
        // difference sync doubling it; notices carry none.
        platform_msg_id: ev.platform_msg_id,
        attachments: Vec::new(),
        created_at: ev.created_at,
        steps: Vec::new(),
        work_started_at: None,
        work_ended_at: None,
        cancelled: false,
        turn_complete: None,
        notice_level: level.map(str::to_owned),
    }
}

/// Concatenate the visible text of a model thinking block (redacted
/// reasoning carries no display text and is skipped).
///
/// Segments are separated by a BLANK line. Each one is its own section and
/// typically opens with a `**Headline**`; the client renders this as markdown,
/// and CommonMark folds a lone newline into a space — which glues the headline
/// onto the tail of the previous section's last sentence
/// (`…I need!**Inspecting the repo**`). A multi-segment block only ever comes
/// from the non-streaming completion path, so this reconstruction is the only
/// place that boundary can be preserved or lost.
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
            out.push_str("\n\n");
        }
        out.push_str(part);
    }
    out
}

/// Best-effort short label for a tool call, pulled from a common input key
/// (path / command / url / query). Stands in for the live `progress_label`,
/// which needs the tool registry that isn't on the read path. `None` when
/// nothing recognizable is present.
fn tool_label(tool: &str, input: &serde_json::Value) -> Option<String> {
    const KEYS: [&str; 6] = ["command", "url", "path", "file_path", "query", "pattern"];
    /// `spawn_subagent` carries none of the generic keys, so a delegation
    /// rendered as a bare tool name with no hint of who was sent to do what.
    /// Its `description` IS the errand — the same string the child session's
    /// title is stamped with at spawn — and the profile name is the fallback.
    /// Scoped to this one tool rather than added to `KEYS`: `description` is a
    /// common parameter name and would relabel unrelated calls.
    const SPAWN_KEYS: [&str; 2] = ["description", "subagent_type"];
    let obj = input.as_object()?;
    let keys: &[&str] = if tool == baybo_model::SPAWN_SUBAGENT_TOOL_NAME {
        &SPAWN_KEYS
    } else {
        &KEYS
    };
    for key in keys.iter().copied() {
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
    } else if inner.starts_with(TOOL_RESULT_ERROR_PREFIX) {
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
            platform_msg_id: String::new(),
        }
    }

    #[test]
    fn tool_label_names_the_subagent_errand() {
        let input = serde_json::json!({
            "subagent_type": "explorer",
            "description": "search the sync protocol",
            "prompt": "a long self-contained instruction",
        });
        assert_eq!(
            tool_label(baybo_model::SPAWN_SUBAGENT_TOOL_NAME, &input).as_deref(),
            Some("search the sync protocol"),
            "a delegation step must say what was delegated"
        );
        // Falls back to the profile when the model omitted a description.
        let bare = serde_json::json!({ "subagent_type": "planner" });
        assert_eq!(
            tool_label(baybo_model::SPAWN_SUBAGENT_TOOL_NAME, &bare).as_deref(),
            Some("planner")
        );
        // The spawn keys are scoped to that tool — `description` is a common
        // parameter name and must not relabel unrelated calls.
        assert_eq!(
            tool_label(
                "cron_create",
                &serde_json::json!({"description": "nightly"})
            ),
            None
        );
    }

    #[test]
    fn a_brief_reconstructs_as_the_ask_it_answers() {
        // One reading, not one per reader: the only transcripts holding a
        // brief are issue runs', so there is nothing for a knob to choose
        // between — and the framing the prompt module wrapped it in comes back
        // off, because that half is written for the model.
        let tail = vec![
            (
                1,
                ts(1),
                ChatMessage::issue_brief(vec![text(
                    &baybo_context::prompts::issue::frame_issue_brief(
                        7,
                        "/ws/work/projects/p/7",
                        "fix the retry",
                    ),
                )]),
            ),
            (
                2,
                ts(4),
                ChatMessage::assistant(vec![
                    text("looking"),
                    tool_use("c1", "Read", serde_json::json!({"path": "/x"})),
                ]),
            ),
            (
                3,
                ts(6),
                ChatMessage::tool_result("c1".to_owned(), "ok".to_owned()),
            ),
            (4, ts(9), ChatMessage::assistant(vec![text("fixed")])),
        ];

        let items = reconstruct_transcript(tail, Vec::new(), None, Vec::new());
        assert!(matches!(items[0].kind, TranscriptItemKind::Message));
        assert_eq!(items[0].role, "user");
        assert_eq!(items[0].text, "fix the retry", "the ask, not the framing");
        // And it opens the turn, so the run's work is timed from the moment it
        // was asked rather than from its first persisted iteration.
        assert!(matches!(items[1].kind, TranscriptItemKind::Work));
        assert_eq!(items[1].work_started_at, Some(ts(1)));
    }

    #[test]
    fn control_event_item_emits_the_command_platform_msg_id() {
        // A `/compact` echo reconstructs as a user MESSAGE row carrying the
        // send's `platform_msg_id`, so a client's optimistic command bubble
        // reconciles with it instead of a difference sync doubling the bubble.
        let mut cmd = ctl(1, 4, baybo_model::ControlEventKind::Command, "/compact", 5);
        cmd.platform_msg_id = "pm-7".to_owned();
        let item = control_event_item(cmd);
        assert_eq!(item.id, "n1");
        assert_eq!(item.role, "user");
        assert_eq!(item.platform_msg_id, "pm-7");

        // A notice reconstructs with no msg id (it is not a user send).
        let notice = ctl(
            2,
            4,
            baybo_model::ControlEventKind::NoticeInfo,
            "Stopped",
            5,
        );
        assert_eq!(control_event_item(notice).platform_msg_id, "");
    }

    #[test]
    fn reconstruct_folds_progress_control_event_into_work_block() {
        use baybo_model::ControlEventKind::Progress;
        let tail = vec![
            (2, ts(2), ChatMessage::user(vec![text("go")])),
            (
                3,
                ts(3),
                ChatMessage::assistant(vec![
                    thinking("planning"),
                    tool_use("c1", "Bash", serde_json::json!({"command": "ls"})),
                ]),
            ),
            (
                4,
                ts(5),
                ChatMessage::tool_result(
                    "c1".to_owned(),
                    "<tool_output name=\"Bash\">ok</tool_output>".to_owned(),
                ),
            ),
            (5, ts(7), ChatMessage::assistant(vec![text("done")])),
        ];
        // A progress line fired mid-turn, anchored after the tool iteration row.
        let control = vec![ctl(1, 3, Progress, "Running ls", 4)];
        let items = reconstruct_transcript(tail, control, None, Vec::new());

        // Three items — the progress line folds INTO the work block, it is NOT
        // its own row.
        assert_eq!(items.len(), 3, "user + work + answer: {items:?}");
        let work = &items[1];
        assert!(matches!(work.kind, TranscriptItemKind::Work));
        assert_eq!(
            work.steps.len(),
            3,
            "reasoning + tool + status: {:?}",
            work.steps
        );
        assert!(matches!(work.steps[0].kind, WorkStepKind::Reasoning));
        assert!(matches!(work.steps[1].kind, WorkStepKind::Tool));
        assert!(matches!(work.steps[2].kind, WorkStepKind::Status));
        assert_eq!(work.steps[2].text, "Running ls");
        assert!(
            !items
                .iter()
                .any(|i| matches!(i.kind, TranscriptItemKind::Notice)),
            "a progress control event must not surface as a notice row"
        );
    }

    #[test]
    fn reconstruct_splits_a_work_block_across_a_compaction_watermark() {
        // A MID-TURN compaction: the agent's turn ran intermediate rows 1..=2,
        // then compaction fired (watermark 3, its machinery 3..=9 hidden from
        // the display), then the SAME turn resumed at 10..=11 and answered at
        // 12. The displayed rows straddle the watermark with NO user/answer row
        // between — the exact case that folds into one card and swallows the
        // pre-compaction divider.
        let tail = vec![
            (0, ts(0), ChatMessage::user(vec![text("go")])),
            (
                1,
                ts(1),
                ChatMessage::assistant(vec![
                    thinking("plan"),
                    tool_use("c1", "Bash", serde_json::json!({"command": "ls"})),
                ]),
            ),
            (
                2,
                ts(2),
                ChatMessage::tool_result("c1".to_owned(), "ok".to_owned()),
            ),
            // ── compaction watermark 3; machinery 3..=9 elided from display ──
            (
                10,
                ts(10),
                ChatMessage::assistant(vec![
                    thinking("resume"),
                    tool_use("c2", "Bash", serde_json::json!({"command": "pwd"})),
                ]),
            ),
            (
                11,
                ts(11),
                ChatMessage::tool_result("c2".to_owned(), "ok".to_owned()),
            ),
            (12, ts(12), ChatMessage::assistant(vec![text("done")])),
        ];
        let attachments = HashMap::new();

        // Without the watermark the two halves fold into ONE spanning card.
        let fused = reconstruct_transcript_with_attachments(
            tail.clone(),
            Vec::new(),
            None,
            Vec::new(),
            &attachments,
            &[],
        );
        let fused_work = fused
            .iter()
            .filter(|i| matches!(i.kind, TranscriptItemKind::Work))
            .count();
        assert_eq!(fused_work, 1, "no watermark ⇒ one spanning card: {fused:?}");

        // With the watermark, the block breaks at the seam into two cards; the
        // pre-compaction half is `turn_complete` so the client never re-fuses
        // it, and the divider lands between w1 and w10.
        let split = reconstruct_transcript_with_attachments(
            tail,
            Vec::new(),
            None,
            Vec::new(),
            &attachments,
            &[3],
        );
        let work: Vec<&ChatTranscriptItem> = split
            .iter()
            .filter(|i| matches!(i.kind, TranscriptItemKind::Work))
            .collect();
        assert_eq!(work.len(), 2, "watermark ⇒ split into two: {split:?}");
        assert_eq!(
            work[0].id, "w1",
            "pre-compaction half keyed by its first row"
        );
        assert_eq!(
            work[0].turn_complete,
            Some(true),
            "closed at the seam, not cut by a page edge"
        );
        assert_eq!(work[1].id, "w10", "post-compaction half opens fresh");
    }

    #[test]
    fn reconstruct_dedups_progress_against_in_flight_status() {
        use baybo_model::ControlEventKind::Progress;
        // An in-flight turn: one persisted tool iteration, a progress line that
        // BOTH persisted (control event) AND still sits in the live buffer.
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
        ];
        let control = vec![ctl(1, 3, Progress, "narrate", 4)];
        // The channel's in-flight buffer still holds the same narration line
        // plus a fresher reasoning step not yet persisted.
        let in_flight = vec![
            ChatWorkStep::status("narrate".to_owned()),
            ChatWorkStep::reasoning("more".to_owned()),
        ];
        let items = reconstruct_transcript(tail, control, Some(ts(2)), in_flight);

        let work = items
            .iter()
            .find(|i| matches!(i.kind, TranscriptItemKind::Work))
            .expect("a trailing work block");
        let status_count = work
            .steps
            .iter()
            .filter(|s| s.kind == WorkStepKind::Status && s.text == "narrate")
            .count();
        assert_eq!(
            status_count, 1,
            "the duplicate in-flight status is dropped: {:?}",
            work.steps
        );
        assert!(
            work.steps
                .iter()
                .any(|s| s.kind == WorkStepKind::Reasoning && s.text == "more"),
            "a non-duplicate in-flight step still appends: {:?}",
            work.steps
        );
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
        assert_eq!(
            work.turn_complete,
            Some(true),
            "the final reply closed the turn in-window — a whole block, never fused with a neighbour"
        );
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
        // TurnState instant (ts(9), the turn start) — NOT the user message's
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

    /// A reconstructed tool step carries the SAME call id the live frames do.
    /// It is the step's identity: without it every reconstructed call in a
    /// block looks alike, so a client folding two reconstructions of one turn
    /// (routine — a turn longer than a page reconstructs per-page) cannot tell
    /// them apart, and one folding a live block with its own reconstruction
    /// double-renders every call.
    #[test]
    fn reconstruct_tool_steps_carry_their_call_id() {
        let tail = vec![
            (2, ts(2), ChatMessage::user(vec![text("go")])),
            (
                3,
                ts(3),
                ChatMessage::assistant(vec![
                    tool_use("call_a", "Bash", serde_json::json!({"command": "ls"})),
                    tool_use("call_b", "Bash", serde_json::json!({"command": "pwd"})),
                ]),
            ),
            (4, ts(6), ChatMessage::assistant(vec![text("done")])),
        ];
        let items = reconstruct_transcript(tail, Vec::new(), None, Vec::new());

        let work = items
            .iter()
            .find(|i| matches!(i.kind, TranscriptItemKind::Work))
            .expect("work block");
        let ids: Vec<_> = work.steps.iter().map(|s| s.call_id.as_deref()).collect();
        assert_eq!(ids, vec![Some("call_a"), Some("call_b")], "{work:?}");
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
            ChatWorkStep::tool("c1".into(), "Bash".into(), Some("ls".into())),
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
    fn reconstruct_drops_an_in_flight_buffer_its_own_turn_already_outlived() {
        // The finalization window: the answer row is persisted (so the turn's
        // card is reconstructed above it) but the turn's `Message` fan-out has
        // not cleared the live channel's buffer yet, so the SAME work is
        // available twice. Folding it emits the card a second time below the
        // reply — seeded from `last_ordinal`, i.e. that reply's own ordinal.
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
            (5, ts(5), ChatMessage::assistant(vec![text("here you go")])),
        ];
        // The channel stamps every buffered step when it records it, and all of
        // this turn's landed before its answer row was written.
        let in_flight = vec![
            ChatWorkStep::reasoning("weighing the options".into()).stamped(ts(3)),
            ChatWorkStep::tool("c1".into(), "Bash".into(), Some("ls".into())).stamped(ts(4)),
        ];
        let items = reconstruct_transcript(tail, Vec::new(), Some(ts(1)), in_flight);

        assert_eq!(
            items
                .iter()
                .filter(|i| matches!(i.kind, TranscriptItemKind::Work))
                .count(),
            1,
            "one turn, one card: {items:?}"
        );
        let last = items.last().expect("items");
        assert!(
            matches!(last.kind, TranscriptItemKind::Message) && last.role == "assistant",
            "the reply ends the thread, nothing below it: {items:?}"
        );
    }

    #[test]
    fn reconstruct_keeps_an_in_flight_buffer_from_a_turn_started_after_the_answer() {
        // The other side of the same test: a turn that began AFTER the newest
        // persisted answer has work no page row can show yet, so its buffered
        // steps still surface as the trailing (cut-off) block.
        let tail = vec![
            (2, ts(2), ChatMessage::user(vec![text("go")])),
            (3, ts(3), ChatMessage::assistant(vec![text("done")])),
        ];
        let in_flight =
            vec![ChatWorkStep::reasoning("a later turn thinking".into()).stamped(ts(9))];
        let items = reconstruct_transcript(tail, Vec::new(), Some(ts(9)), in_flight);

        let last = items.last().expect("items");
        assert!(
            matches!(last.kind, TranscriptItemKind::Work),
            "the later turn keeps its trailing block: {items:?}"
        );
        assert_eq!(last.work_started_at, Some(ts(9)));
        assert_eq!(
            last.turn_complete,
            Some(false),
            "a trailing block declares itself cut off"
        );
    }

    #[test]
    fn reconstruct_does_not_stamp_a_lingering_turn_start_on_a_block_below_the_answer() {
        use baybo_model::ControlEventKind::Progress;
        // A block that opens BELOW the newest answer (its first durable artifact
        // is a progress event, and control events sort after the row they anchor
        // to) belongs to whatever comes next — not to the turn whose
        // `active_turn_started_at` is still lingering through finalization above
        // it. Stamping that start here hands the client a reopen key
        // (`workStartedAt === startedAt`) for the wrong turn.
        let tail = vec![
            (2, ts(2), ChatMessage::user(vec![text("go")])),
            (3, ts(3), ChatMessage::assistant(vec![text("done")])),
        ];
        let events = vec![ctl(0, 3, Progress, "narrating", 5)];
        let items = reconstruct_transcript(tail, events, Some(ts(1)), Vec::new());

        let work = items
            .iter()
            .find(|i| matches!(i.kind, TranscriptItemKind::Work))
            .expect("the trailing progress block");
        assert_eq!(
            work.work_started_at,
            Some(ts(5)),
            "keeps its own anchor, not the finished turn's start: {items:?}"
        );
    }

    #[test]
    fn reconstruct_drops_a_stale_buffer_even_once_the_next_turn_has_started() {
        // The latch and the buffer come from two different places read at two
        // different moments, so the NEXT turn's start is visible here while the
        // PREVIOUS turn's steps are still sitting in the buffer. Judged as a
        // whole the buffer would look live; judged per step it is what it is —
        // work that happened before the answer above, already on the page.
        let tail = vec![
            (2, ts(2), ChatMessage::user(vec![text("go")])),
            (3, ts(3), ChatMessage::assistant(vec![text("done")])),
        ];
        let stale = vec![ChatWorkStep::reasoning("turn A thinking".into()).stamped(ts(1))];
        let items = reconstruct_transcript(tail, Vec::new(), Some(ts(9)), stale);

        assert!(
            !items
                .iter()
                .any(|i| matches!(i.kind, TranscriptItemKind::Work)),
            "the previous turn's leftovers are not the new turn's card: {items:?}"
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
        assert_eq!(
            work.turn_complete,
            Some(false),
            "no boundary in-window — the page edge cut the turn off, so the client fuses it"
        );
        assert_eq!(work.steps.len(), 1);
        assert_eq!(work.steps[0].tool_status.as_deref(), Some("ok"));
        assert_eq!(work.steps[0].tool_summary.as_deref(), Some("ok output"));
        assert_eq!(work.steps[0].approval, None, "no prompt, no label");
    }

    #[test]
    fn reconstruct_reads_approval_off_tool_result_meta() {
        let tail = vec![
            (2, ts(2), ChatMessage::user(vec![text("go")])),
            (
                3,
                ts(3),
                ChatMessage::assistant(vec![
                    tool_use("c1", "Bash", serde_json::json!({"command": "ls"})),
                    tool_use("c2", "Bash", serde_json::json!({"command": "rm x"})),
                ]),
            ),
            (
                4,
                ts(4),
                ChatMessage::tool_result_with_meta(
                    "c1".to_owned(),
                    "ok output".to_owned(),
                    Some(baybo_model::ToolResultMeta {
                        approval: Some(baybo_tools::ApprovalDecision::ApproveAlways),
                        ..Default::default()
                    }),
                ),
            ),
            // A denied row persisted BEFORE the meta field existed: the
            // status sniff still fires, the approval label stays absent.
            (
                5,
                ts(5),
                ChatMessage::tool_result(
                    "c2".to_owned(),
                    "The user explicitly denied permission for tool 'Bash'.".to_owned(),
                ),
            ),
        ];
        let items = reconstruct_transcript(tail, Vec::new(), None, Vec::new());
        let work = &items[1];
        assert_eq!(work.steps.len(), 2);
        assert_eq!(work.steps[0].tool_status.as_deref(), Some("ok"));
        assert_eq!(work.steps[0].approval.as_deref(), Some("approve_always"));
        assert_eq!(work.steps[1].tool_status.as_deref(), Some("denied"));
        assert_eq!(work.steps[1].approval, None);
    }

    #[test]
    fn a_recorded_denial_outranks_the_text_sniff() {
        // A tool that prompted MID-CALL folds the refusal into its own error
        // message, which carries none of the sentinel wording — so the sniff
        // alone reconstructs it as a plain failure while the live view showed
        // it denied. The recorded decision settles it.
        let tail = vec![
            (2, ts(2), ChatMessage::user(vec![text("go")])),
            (
                3,
                ts(3),
                ChatMessage::assistant(vec![tool_use("c1", "Skill", serde_json::json!({}))]),
            ),
            (
                4,
                ts(4),
                ChatMessage::tool_result_with_meta(
                    "c1".to_owned(),
                    "Error: skill 'x' requires env-var approval".to_owned(),
                    Some(baybo_model::ToolResultMeta {
                        approval: Some(ApprovalDecision::Deny),
                        ..Default::default()
                    }),
                ),
            ),
        ];
        let items = reconstruct_transcript(tail, Vec::new(), None, Vec::new());
        let work = &items[1];
        assert_eq!(work.steps[0].tool_status.as_deref(), Some("denied"));
        assert_eq!(work.steps[0].approval.as_deref(), Some("deny"));
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
        assert_eq!(thinking_text(&blocks), "step 1\n\ntl;dr");
    }

    /// The shape the store is full of on the non-streaming path: a segment ends
    /// mid-sentence and the next opens with a bold headline. A lone newline
    /// between them is folded to a space by the client's markdown parser, which
    /// renders `…I need!**Inspecting the repo**` — the headline swallowed by the
    /// previous paragraph.
    #[test]
    fn thinking_text_separates_sections_with_a_blank_line() {
        let blocks = vec![
            ThinkingContent::Summary {
                text: "I want to look at the globs I need!".to_owned(),
            },
            ThinkingContent::Summary {
                text: "**Inspecting the repo**\n\nI need to inspect it.".to_owned(),
            },
        ];
        assert!(
            thinking_text(&blocks).contains("I need!\n\n**Inspecting the repo**"),
            "got {:?}",
            thinking_text(&blocks)
        );
    }

    #[test]
    fn tool_label_pulls_first_known_key() {
        assert_eq!(
            tool_label("bash", &serde_json::json!({"command": "ls -la"})).as_deref(),
            Some("ls -la")
        );
        assert_eq!(
            tool_label("read", &serde_json::json!({"path": "/a/b"})).as_deref(),
            Some("/a/b")
        );
        assert_eq!(tool_label("read", &serde_json::json!({"other": "x"})), None);
        assert_eq!(
            tool_label("read", &serde_json::json!("not an object")),
            None
        );
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
