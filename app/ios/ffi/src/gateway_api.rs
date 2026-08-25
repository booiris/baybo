//! Typed gateway API calls shared by direct REST and relay API-tunnel transports.

use std::future::Future;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::api::{
    ChatSearchGroup, ChatSearchHit, ChatSearchResults, ChatSessionSummary, ChatSubagentList,
    ChatSubagentStatus, ChatSubagentSummary, CronJobStatus, CronJobSummary, DeckCardInfo,
    DeckLayoutEntryInput, DeckSnapshotInfo, DeckView, HiredBy, IssueAttachmentInfo,
    IssueAttachmentInput, IssueInfo, IssuePriority, IssueRunInfo, IssueStatus, LlmModelCatalog,
    LlmModelInfo, ProjectActivity, ProjectAttention, ProjectInfo, RunStatus, RunTrigger,
    SessionModelPin, SubIssueProgress, SubagentCursor, TeamMemberInfo,
};

const PATH_CHAT_SESSIONS: &str = "/v1/chat/sessions";
/// The read-only door onto a subagent child's transcript. A child session is
/// invisible to `/v1/chat/sessions` (its channel is `subagent`, not `owner`),
/// so its reads ride their own lineage-scoped path space.
const PATH_CHAT_SUBAGENTS: &str = "/v1/chat/subagents";
const PATH_CHAT_SEARCH: &str = "/v1/chat/search";
const PATH_CRON: &str = "/v1/cron";
const PATH_LLM_MODELS: &str = "/v1/llm/models";
const PATH_AGENTS: &str = "/v1/agents";
const PATH_DECK: &str = "/v1/deck";
/// The kanban boards. Every card, run, comment and approval on the phone
/// rides this path space — see `app/ios/docs/projects.md`.
const PATH_PROJECTS: &str = "/v1/projects";
const PATH_MOBILE_APNS_TOKEN: &str = "/v1/mobile/apns-token";
pub(crate) const PATH_BLOBS: &str = "/v1/blobs";
/// Content-type for every JSON-bodied request, shared by both legs.
pub(crate) const MEDIA_TYPE_JSON: &str = "application/json";

pub(crate) trait GatewayJsonClient {
    fn get_json<'a, T>(
        &'a self,
        path: &'a str,
    ) -> impl Future<Output = Result<T, String>> + Send + 'a
    where
        T: DeserializeOwned + Send + 'static;

    fn post_json<'a, T>(
        &'a self,
        path: &'a str,
        body: Vec<u8>,
    ) -> impl Future<Output = Result<T, String>> + Send + 'a
    where
        T: DeserializeOwned + Send + 'static;

    /// PATCH a sparse body and decode what came back.
    ///
    /// Distinct from [`Self::put_empty`] in both halves: a board's card is
    /// edited field by field (an absent key means "leave it"), and the
    /// gateway answers with the whole updated card, which is what the
    /// caller renders instead of refetching.
    fn patch_json<'a, T>(
        &'a self,
        path: &'a str,
        body: Vec<u8>,
    ) -> impl Future<Output = Result<T, String>> + Send + 'a
    where
        T: DeserializeOwned + Send + 'static;

    fn post_empty<'a>(
        &'a self,
        path: &'a str,
        body: Vec<u8>,
    ) -> impl Future<Output = Result<(), String>> + Send + 'a;

    fn put_empty<'a>(
        &'a self,
        path: &'a str,
        body: Vec<u8>,
    ) -> impl Future<Output = Result<(), String>> + Send + 'a;

    fn delete_empty<'a>(
        &'a self,
        path: &'a str,
    ) -> impl Future<Output = Result<(), String>> + Send + 'a;

    /// POST `body` verbatim (content-type JSON) and return the raw response
    /// body bytes UNPARSED — the deck's per-card op surface, whose response
    /// shape belongs to the card, not this client. `retryable` is the op's
    /// MANDATORY `x-baybo-retryable` declaration, compiled from the card's
    /// spec at install and served on the deck view: the relay impl replays a
    /// silent pooled leg only when it is true. An undeclared-false op runs
    /// agent-written service code with unrestricted semantics, where the
    /// client-keyed convergence argument in `relay::api::should_retry` does
    /// not hold.
    fn post_raw<'a>(
        &'a self,
        path: &'a str,
        body: Vec<u8>,
        retryable: bool,
    ) -> impl Future<Output = Result<Vec<u8>, String>> + Send + 'a;
}

/// Sent by a companion device when the upload is a deck card's picker choice:
/// the gateway stamps the blob `deck:<card_id>` (not `device:<id>`) so card
/// purge reclaims it. Wire contract mirrored in the gateway's
/// `crates/gateway/src/channel/blobs.rs`.
pub(crate) const HEADER_DECK_CARD: &str = "x-baybo-deck-card";

pub(crate) trait GatewayBlobClient {
    /// `deck_card = Some(card_id)` for a deck picker upload (stamps
    /// `deck:<card_id>`); `None` for an ordinary chat attachment.
    fn upload_blob(
        &self,
        bytes: Vec<u8>,
        mime_type: String,
        deck_card: Option<String>,
    ) -> impl Future<Output = Result<String, String>> + Send + '_;

    /// Path-based twin of [`Self::upload_blob`] for a file the user picked: the
    /// bytes are streamed off disk, so a 100 MiB attachment never crosses the
    /// FFI nor sits in memory whole. `progress` ticks while the body flows.
    fn upload_blob_file(
        &self,
        path: String,
        mime_type: String,
        deck_card: Option<String>,
        progress: crate::blob_helper::ProgressSink,
    ) -> impl Future<Output = Result<String, String>> + Send + '_;

    fn download_blob(
        &self,
        blob_id: String,
        progress: crate::blob_helper::ProgressSink,
    ) -> impl Future<Output = Result<Vec<u8>, String>> + Send + '_;
}

#[derive(Deserialize)]
struct ChatSessionCreated {
    session_id: String,
}

#[derive(Serialize)]
struct CreateSessionRequest<'a> {
    session_id: &'a str,
}

#[derive(Deserialize)]
struct ChatSessionsList {
    items: Vec<SessionSummary>,
}

#[derive(Deserialize)]
struct SessionSummary {
    session_id: String,
    created_at: String,
    last_active: String,
    #[serde(default)]
    last_user_text: Option<String>,
    /// Newest-message preview (any author) — absent on an older gateway.
    #[serde(default)]
    last_message_text: Option<String>,
    /// Auto-generated title — absent until the title pass has run.
    #[serde(default)]
    title: Option<String>,
    pinned: bool,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    unread_count: i64,
    /// The cron job this row is a fire of — the chat list's grouping key. Absent
    /// on an ordinary chat, and on a gateway that predates cron groups.
    #[serde(default)]
    cron_job_id: Option<String>,
    /// The group's label (the job's live title, else the fire's snapshot).
    #[serde(default)]
    cron_job_title: Option<String>,
    /// Whether the job's GROUP is pinned (`cron_jobs.pinned`, read off the live
    /// job). Every fire of the job carries the same value; the client folds it
    /// into the one group row. Distinct from `pinned`, this row's own pin.
    #[serde(default)]
    cron_group_pinned: bool,
    /// A tool call in this conversation is parked on the approval gate.
    /// `#[serde(default)]` so an older gateway (which omits the key entirely,
    /// and one that predates the field) reads as "nothing is waiting".
    #[serde(default)]
    approval_pending: bool,
}

#[derive(Deserialize)]
struct CronJobsList {
    items: Vec<WireCronJob>,
}

/// The gateway's `CronJob` DTO, narrowed to what the phone's list reads. The
/// fields it drops (`user_id`, `channel`, `created_at`, `updated_at`,
/// `origin_session_id`, `pinned`, `deleted_at`) are either constant for this
/// caller or another surface's concern; `deleted_at` in particular can never
/// arrive, because the request asks for the live list.
#[derive(Deserialize)]
struct WireCronJob {
    id: String,
    /// Empty on rows minted before the field existed.
    #[serde(default)]
    title: String,
    prompt: String,
    schedule: WireCronSchedule,
    timezone: String,
    status: WireCronStatus,
    #[serde(default)]
    next_trigger_at: Option<String>,
    #[serde(default)]
    last_triggered_at: Option<String>,
    /// Whether the runtime owns this job. Such a job pauses and reschedules
    /// like any other but refuses `DELETE`, so the row must not offer one.
    /// `#[serde(default)]`: absent from a gateway older than the field.
    #[serde(default)]
    builtin: bool,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireCronSchedule {
    Cron {
        expr: String,
    },
    /// A one-shot. Its `time` is deliberately not captured: the variant exists to
    /// be RECOGNISED so `list_cron_jobs` can drop the row, and a field nothing
    /// reads is a field that goes stale. Serde ignores the rest of the object.
    At,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireCronStatus {
    Enabled,
    Disabled,
    Executed,
}

/// The gateway's `DeckCardDto` (`GET /v1/deck`, `GET /v1/deck/recycle`,
/// `POST /v1/deck/cards/{id}/restore`). `deleted_at_ms` is skipped server-side
/// on live rows, hence the default.
#[derive(Deserialize)]
struct WireDeckCard {
    card_id: String,
    title: String,
    position: i64,
    size: String,
    #[serde(default)]
    sizes: Vec<String>,
    #[serde(default)]
    maximize: bool,
    enabled: bool,
    quarantined: bool,
    #[serde(default)]
    deleted_at_ms: Option<i64>,
    spec_hash: String,
    last_seq: i64,
    created_at_ms: i64,
    #[serde(default)]
    retryable_ops: Vec<String>,
}

impl WireDeckCard {
    fn into_info(self) -> DeckCardInfo {
        // A pre-field gateway (or a card that never declared sizes) omits the
        // set — fall back to `[size]` so the client always has a non-empty
        // list that contains the current size.
        let sizes = if self.sizes.is_empty() {
            vec![self.size.clone()]
        } else {
            self.sizes
        };
        DeckCardInfo {
            card_id: self.card_id,
            title: self.title,
            position: self.position,
            size: self.size,
            sizes,
            maximize: self.maximize,
            enabled: self.enabled,
            quarantined: self.quarantined,
            deleted_at_ms: self.deleted_at_ms,
            spec_hash: self.spec_hash,
            last_seq: self.last_seq,
            created_at_ms: self.created_at_ms,
            retryable_ops: self.retryable_ops,
        }
    }
}

/// The gateway's `DeckSnapshotDto`. `error` is skipped server-side on a clean
/// snapshot, hence the default.
#[derive(Deserialize)]
struct WireDeckSnapshot {
    card_id: String,
    seq: i64,
    payload: String,
    fetched_at_ms: i64,
    #[serde(default)]
    error: Option<String>,
}

impl WireDeckSnapshot {
    fn into_info(self) -> DeckSnapshotInfo {
        DeckSnapshotInfo {
            card_id: self.card_id,
            seq: self.seq,
            payload: self.payload,
            fetched_at_ms: self.fetched_at_ms,
            error: self.error,
        }
    }
}

/// The gateway's `DeckResponse` (`GET /v1/deck`).
#[derive(Deserialize)]
struct WireDeckResponse {
    cards: Vec<WireDeckCard>,
    snapshots: Vec<WireDeckSnapshot>,
}

/// The gateway's `DeckBundleDto` (`GET /v1/deck/cards/{id}/bundle`).
#[derive(Deserialize)]
struct WireDeckBundle {
    card_html: String,
}

/// One `PUT /v1/deck/layout` entry — the gateway's `DeckLayoutEntryDto`.
#[derive(Serialize)]
struct WireDeckLayoutEntry<'a> {
    card_id: &'a str,
    position: i64,
    size: &'a str,
}

#[derive(Serialize)]
struct SetArchivedRequest {
    archived: bool,
}

#[derive(Serialize)]
struct SetPinnedRequest {
    pinned: bool,
}

#[derive(Serialize)]
struct SetTitleRequest<'a> {
    title: &'a str,
}

#[derive(Serialize)]
struct MarkReadRequest {
    ordinal: i64,
}

#[derive(Serialize)]
struct MarkManyReadRequest {
    session_ids: Vec<String>,
}

#[derive(Serialize)]
struct HideManyRequest {
    session_ids: Vec<String>,
}

/// Backward page of the transcript (`GET /v1/chat/sessions/{id}`). The rows
/// are the gateway's full-fidelity `ChatTranscriptItem` DTOs (message | work |
/// notice, keyed by their stable `id`); they pass through to the webview
/// verbatim — NO client-side filtering (v2 contract: fidelity is a property
/// of the data, never of the path that fetched it).
#[derive(Deserialize)]
struct ChatSessionDetail {
    transcript: Vec<serde_json::Value>,
    has_more: bool,
    #[serde(default)]
    oldest_ordinal: Option<i64>,
    #[serde(default)]
    newest_ordinal: Option<i64>,
}

/// Native-synthesized frame for the web transcript bridge: one backward
/// history page. `rows` are verbatim `ChatTranscriptItem`s.
#[derive(Serialize)]
struct HistoryPageFrame {
    kind: &'static str,
    rows: Vec<serde_json::Value>,
    oldest_ordinal: Option<i64>,
    newest_ordinal: Option<i64>,
    has_more: bool,
}

/// One context-compaction boundary (`ChatSessionDetail.compaction_points` /
/// `ChatSyncResponse.compaction_points`): the summary-head `ordinal` and the
/// compaction time `at`. Passed verbatim to the webview, which draws a
/// pre-compaction divider before the first row at/after `ordinal`.
#[derive(Deserialize, Serialize)]
struct CompactionPoint {
    ordinal: i64,
    at: String,
}

/// `GET /v1/chat/sessions/{id}/sync` — the one forward-recovery pull.
#[derive(Deserialize)]
struct ChatSyncResponse {
    rows: Vec<serde_json::Value>,
    #[serde(default)]
    next_cursor: Option<i64>,
    rebased: bool,
    #[serde(default)]
    oldest_ordinal: Option<i64>,
    has_more_older: bool,
    /// Compaction boundaries, carried on every sync so the webview can draw
    /// the pre-compaction divider without a separate meta fetch.
    #[serde(default)]
    compaction_points: Vec<CompactionPoint>,
}

/// Native-synthesized frame for the web transcript bridge: one sync page.
/// `since_ordinal` echoes the request's cursor so the web side can tell a
/// baseline REPLACE (`null`) from a difference merge without extra state.
/// Option fields serialize as explicit `null` on purpose — the web handler
/// reads them directly.
#[derive(Serialize)]
struct SyncPageFrame {
    kind: &'static str,
    rows: Vec<serde_json::Value>,
    since_ordinal: Option<i64>,
    next_cursor: Option<i64>,
    rebased: bool,
    oldest_ordinal: Option<i64>,
    has_more_older: bool,
    /// Omitted (not `[]`) for a never-compacted session, so the common frame
    /// shape is unchanged; the webview reads it as an optional field.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    compaction_points: Vec<CompactionPoint>,
}

/// `GET /v1/chat/sessions/{id}/subagents` — the gateway's `ChatSubagentList`.
/// The route SKIPS what a child has none of (`subagent_type`, `task`,
/// `started_at`, `ended_at`); the sheet is a display surface, so a key that is
/// absent costs one row its detail rather than blanking the whole listing.
#[derive(Deserialize)]
struct WireSubagentList {
    #[serde(default)]
    items: Vec<WireSubagentSummary>,
    #[serde(default)]
    has_more_older: bool,
}

#[derive(Deserialize)]
struct WireSubagentSummary {
    session_id: String,
    #[serde(default)]
    subagent_type: Option<String>,
    backend: String,
    /// The errand the parent authored, stamped onto the child's title at spawn
    /// — absent on a child spawned before that stamp existed.
    #[serde(default)]
    task: Option<String>,
    status: WireSubagentStatus,
    created_at: String,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    ended_at: Option<String>,
}

/// The gateway's `ChatSubagentStatus`. `Unknown` is both its own drift arm and
/// this side's: a gateway that grows a status must cost one row its label, not
/// fail the listing's decode.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireSubagentStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
    #[serde(other)]
    Unknown,
}

/// `GET /v1/chat/search` — the gateway's grouped result set.
///
/// Every field past `groups` decodes tolerantly: an older gateway that predates
/// a key must degrade to a usable result rather than blanking the whole search.
#[derive(Deserialize)]
struct WireSearchResults {
    #[serde(default)]
    groups: Vec<WireSearchGroup>,
    #[serde(default)]
    truncated: bool,
}

#[derive(Deserialize)]
struct WireSearchGroup {
    session_id: String,
    #[serde(default)]
    session_title: Option<String>,
    #[serde(default)]
    hits: Vec<WireSearchHit>,
    #[serde(default)]
    total_hits: i64,
}

#[derive(Deserialize)]
struct WireSearchHit {
    ordinal: i64,
    role: String,
    #[serde(default)]
    text: String,
    created_at: String,
    #[serde(default)]
    superseded_by: Option<i64>,
}

/// `GET /v1/chat/sessions/{id}/messages?platform_msg_id=…` — the per-send
/// durability point lookup (outbox rule 4: resolve a rebase-floor entry
/// without consuming a retry transmission).
#[derive(Deserialize)]
pub(crate) struct ChatMessageLookupResponse {
    pub(crate) found: bool,
    #[serde(default)]
    pub(crate) ordinal: Option<i64>,
}

/// `GET /v1/llm/models`, narrowed to the picker's fields. The gateway row
/// carries a full dashboard's worth of config/pricing detail — serde drops it.
#[derive(Deserialize)]
struct LlmModelsList {
    default_name: String,
    items: Vec<WireLlmModel>,
}

#[derive(Deserialize)]
struct WireLlmModel {
    name: String,
    provider: String,
    model: String,
    /// The gateway sends every model the entry serves — default included,
    /// each as an object carrying that model's config overrides. Only the
    /// id reaches the picker.
    #[serde(default)]
    model_list: Vec<WireLlmModelSpec>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    /// Thinking levels this entry's provider can actually be told. Empty
    /// means baybo sends this provider no effort, so the panel hides the
    /// Thinking row rather than offering inert picks.
    #[serde(default)]
    available_efforts: Vec<String>,
}

#[derive(Deserialize)]
struct WireLlmModelSpec {
    model: String,
}

/// `GET /v1/chat/sessions/{id}?limit=1` read for the session meta only — the
/// transcript page rides along and is dropped; `limit=1` keeps it one row.
#[derive(Deserialize)]
struct SessionModelMeta {
    #[serde(default)]
    last_llm: Option<String>,
    #[serde(default)]
    last_model: Option<String>,
    #[serde(default)]
    last_effort: Option<String>,
}

/// `PUT /v1/chat/sessions/{id}/model` body. `None`s must serialize as EXPLICIT
/// nulls — `{"llm":null}` is the "clear the pin, follow `default-llm`" request
/// and `{"model":null}` the "use the entry's default model" request; omitting
/// a field means the same today only because the gateway defaults it, and this
/// side should not lean on that.
#[derive(Serialize)]
struct SetSessionModelRequest<'a> {
    llm: Option<&'a str>,
    model: Option<&'a str>,
    reasoning_effort: Option<&'a str>,
}

#[derive(Serialize)]
struct UpdateApnsTokenRequest<'a> {
    apns_token: &'a str,
    apns_env: &'a str,
}

pub(crate) async fn create_session<C: GatewayJsonClient + Sync>(
    client: &C,
    session_id: &str,
) -> Result<String, String> {
    let body = serde_json::to_vec(&CreateSessionRequest { session_id })
        .map_err(|e| format!("encode create session request: {e}"))?;
    let created: ChatSessionCreated = client.post_json(PATH_CHAT_SESSIONS, body).await?;
    Ok(created.session_id)
}

pub(crate) async fn list_sessions<C: GatewayJsonClient + Sync>(
    client: &C,
) -> Result<Vec<ChatSessionSummary>, String> {
    let list: ChatSessionsList = client.get_json(PATH_CHAT_SESSIONS).await?;
    Ok(list
        .items
        .into_iter()
        .map(|s| ChatSessionSummary {
            session_id: s.session_id,
            created_at: s.created_at,
            last_active: s.last_active,
            last_user_text: s.last_user_text,
            last_message_text: s.last_message_text,
            title: s.title,
            pinned: s.pinned,
            archived: s.archived,
            unread_count: s.unread_count,
            cron_job_id: s.cron_job_id,
            cron_job_title: s.cron_job_title,
            cron_group_pinned: s.cron_group_pinned,
            approval_pending: s.approval_pending,
        })
        .collect())
}

/// The owner's LIVE, RECURRING scheduled jobs, in the order the gateway returned
/// them (`GET /v1/cron?channel=owner`).
///
/// Three filters. Two are the server's, one is ours:
///
/// - **live only** — the request's default. The recycle bin is a desktop
///   affordance (it needs restore, which this list does not offer), so a phone
///   that listed deleted jobs could only tease them.
/// - **`channel=owner`** — the one channel whose fires this app can open. The
///   route is otherwise an unfiltered operator view spanning every channel, and
///   a `telegram` job's prompt has no business crossing to a phone that can
///   neither show its history nor act on it.
/// - **recurring only** — dropped HERE rather than asked of the gateway, because
///   unlike the other two it protects nothing and hides nothing: it is this
///   list's identity. A one-shot (`{"kind":"at"}`) is a reminder — it fires once
///   and is over — while this list is what runs on a repeat, and every verb it
///   offers (pause, resume) is meaningless on a moment that has passed.
pub(crate) async fn list_cron_jobs<C: GatewayJsonClient + Sync>(
    client: &C,
) -> Result<Vec<CronJobSummary>, String> {
    // The same const the gateway compares against — this is one channel, named
    // once, not a string the two sides each spell for themselves.
    let path = format!("{PATH_CRON}?channel={}", baybo_model::ChannelType::OWNER);
    let list: CronJobsList = client.get_json(&path).await?;
    Ok(list
        .items
        .into_iter()
        .filter_map(|job| {
            let WireCronSchedule::Cron { expr } = job.schedule else {
                return None;
            };
            Some(CronJobSummary {
                id: job.id,
                title: job.title,
                prompt: job.prompt,
                expr,
                timezone: job.timezone,
                status: match job.status {
                    WireCronStatus::Enabled => CronJobStatus::Enabled,
                    WireCronStatus::Disabled => CronJobStatus::Disabled,
                    WireCronStatus::Executed => CronJobStatus::Executed,
                },
                next_trigger_at: job.next_trigger_at,
                last_triggered_at: job.last_triggered_at,
                builtin: job.builtin,
            })
        })
        .collect())
}

/// The deck view: live cards ordered by position + latest snapshot per card
/// (`GET /v1/deck`). The instant-paint pull behind the deck tab's open and
/// every `DeckChanged` refetch.
pub(crate) async fn fetch_deck<C: GatewayJsonClient + Sync>(
    client: &C,
) -> Result<DeckView, String> {
    let response: WireDeckResponse = client.get_json(PATH_DECK).await?;
    Ok(DeckView {
        cards: response
            .cards
            .into_iter()
            .map(WireDeckCard::into_info)
            .collect(),
        snapshots: response
            .snapshots
            .into_iter()
            .map(WireDeckSnapshot::into_info)
            .collect(),
    })
}

/// The deck's recycle bin — soft-deleted cards, most recent first
/// (`GET /v1/deck/recycle`).
pub(crate) async fn fetch_deck_recycle<C: GatewayJsonClient + Sync>(
    client: &C,
) -> Result<Vec<DeckCardInfo>, String> {
    let cards: Vec<WireDeckCard> = client.get_json(&format!("{PATH_DECK}/recycle")).await?;
    Ok(cards.into_iter().map(WireDeckCard::into_info).collect())
}

/// A card's frontend (`GET /v1/deck/cards/{id}/bundle` → `card_html`). The
/// deck shell holds no gateway URL or token in either mode, so the HTML
/// reaches it over this call like all other data.
pub(crate) async fn fetch_deck_bundle<C: GatewayJsonClient + Sync>(
    client: &C,
    card_id: String,
) -> Result<String, String> {
    validate_path_segment(&card_id, "card_id")?;
    let bundle: WireDeckBundle = client
        .get_json(&format!("{PATH_DECK}/cards/{card_id}/bundle"))
        .await?;
    Ok(bundle.card_html)
}

/// A user-initiated card op (`POST /v1/deck/services/{id}/{op}`): `params_json`
/// goes up verbatim as the JSON body, and the op's JSON result comes back
/// verbatim — the response shape is the card's own contract, never parsed
/// here. Rides [`GatewayJsonClient::post_raw`], the one no-replay route.
pub(crate) async fn call_deck_op<C: GatewayJsonClient + Sync>(
    client: &C,
    card_id: String,
    op: String,
    params_json: String,
    retryable: bool,
) -> Result<String, String> {
    validate_path_segment(&card_id, "card_id")?;
    validate_path_segment(&op, "op")?;
    let path = format!("{PATH_DECK}/services/{card_id}/{op}");
    let body = client
        .post_raw(&path, params_json.into_bytes(), retryable)
        .await?;
    String::from_utf8(body).map_err(|e| format!("decode op response: {e}"))
}

/// Full ordered layout write (`PUT /v1/deck/layout`) — an absolute snapshot of
/// every card's position + size, so a replay converges.
pub(crate) async fn set_deck_layout<C: GatewayJsonClient + Sync>(
    client: &C,
    entries: Vec<DeckLayoutEntryInput>,
) -> Result<(), String> {
    let wire: Vec<WireDeckLayoutEntry<'_>> = entries
        .iter()
        .map(|e| WireDeckLayoutEntry {
            card_id: &e.card_id,
            position: e.position,
            size: &e.size,
        })
        .collect();
    let body = serde_json::to_vec(&wire).map_err(|e| format!("encode deck layout: {e}"))?;
    client.put_empty(&format!("{PATH_DECK}/layout"), body).await
}

/// Run-state toggle (`POST /v1/deck/cards/{id}/enable|disable`). Enabling
/// re-runs the dry-run gate; a failed gate leaves the card quarantined and the
/// error rides back here.
pub(crate) async fn set_deck_enabled<C: GatewayJsonClient + Sync>(
    client: &C,
    card_id: String,
    enabled: bool,
) -> Result<(), String> {
    validate_path_segment(&card_id, "card_id")?;
    let action = if enabled { "enable" } else { "disable" };
    let path = format!("{PATH_DECK}/cards/{card_id}/{action}");
    client.post_empty(&path, Vec::new()).await
}

/// Soft-delete a card into the recycle bin (`DELETE /v1/deck/cards/{id}`) —
/// the service stops, the bundle files stay, restore undoes it.
pub(crate) async fn delete_deck_card<C: GatewayJsonClient + Sync>(
    client: &C,
    card_id: String,
) -> Result<(), String> {
    validate_path_segment(&card_id, "card_id")?;
    let path = format!("{PATH_DECK}/cards/{card_id}");
    client.delete_empty(&path).await
}

/// Restore a card from the recycle bin (`POST /v1/deck/cards/{id}/restore`).
/// The gateway re-runs the dry-run gate first; a failed gate leaves the card
/// in the bin with the error returned here. Success returns the live row.
pub(crate) async fn restore_deck_card<C: GatewayJsonClient + Sync>(
    client: &C,
    card_id: String,
) -> Result<DeckCardInfo, String> {
    validate_path_segment(&card_id, "card_id")?;
    let path = format!("{PATH_DECK}/cards/{card_id}/restore");
    let card: WireDeckCard = client.post_json(&path, Vec::new()).await?;
    Ok(card.into_info())
}

/// Pause or resume a scheduled job (`POST /v1/cron/{id}/pause|resume`).
///
/// Paused, the job keeps its schedule and loses its next trigger; resumed, the
/// gateway recomputes that trigger **from now** and does not backfill the slots
/// it slept through. Resuming a one-shot whose moment has passed is a 400 — its
/// schedule has no future left — which is why the phone offers neither verb on a
/// job that has already run.
pub(crate) async fn set_cron_paused<C: GatewayJsonClient + Sync>(
    client: &C,
    job_id: String,
    paused: bool,
) -> Result<(), String> {
    validate_path_segment(&job_id, "job_id")?;
    let action = if paused { "pause" } else { "resume" };
    let path = format!("{PATH_CRON}/{job_id}/{action}");
    client.post_empty(&path, Vec::new()).await
}

/// Delete a scheduled job (`DELETE /v1/cron/{id}`) — a SOFT delete: the row goes
/// to the recycle bin (`deleted_at`), stops firing, and is restorable from the
/// web dashboard.
///
/// Its execution records are NOT touched. They are ordinary sessions and outlive
/// the job that made them, so the chat list keeps showing that history under the
/// title each fire snapshotted. (Deleting the *group* is the opposite gesture:
/// see `chat_hide_many` — it clears the records and leaves the job running.)
pub(crate) async fn delete_cron_job<C: GatewayJsonClient + Sync>(
    client: &C,
    job_id: String,
) -> Result<(), String> {
    validate_path_segment(&job_id, "job_id")?;
    let path = format!("{PATH_CRON}/{job_id}");
    client.delete_empty(&path).await
}

pub(crate) async fn set_archived<C: GatewayJsonClient + Sync>(
    client: &C,
    session_id: String,
    archived: bool,
) -> Result<(), String> {
    validate_path_segment(&session_id, "session_id")?;
    let body = serde_json::to_vec(&SetArchivedRequest { archived })
        .map_err(|e| format!("encode set archived request: {e}"))?;
    let path = format!("{PATH_CHAT_SESSIONS}/{session_id}/archive");
    client.put_empty(&path, body).await
}

pub(crate) async fn set_pinned<C: GatewayJsonClient + Sync>(
    client: &C,
    session_id: String,
    pinned: bool,
) -> Result<(), String> {
    validate_path_segment(&session_id, "session_id")?;
    let body = serde_json::to_vec(&SetPinnedRequest { pinned })
        .map_err(|e| format!("encode set pinned request: {e}"))?;
    let path = format!("{PATH_CHAT_SESSIONS}/{session_id}/pin");
    client.put_empty(&path, body).await
}

/// Rename a conversation (`PUT /v1/chat/sessions/{id}/title`).
///
/// The gateway normalizes what it stores (interior whitespace collapsed, trimmed,
/// capped at `baybo_model::MAX_SESSION_TITLE_LEN`) and answers 400 for a title
/// that ends up empty or over-long — so the client sends the normalized form it
/// intends to render (`RenameTitle` on the Swift side) rather than raw input, and
/// a 400 here means the two normalizers have drifted apart.
///
/// The response is 204: nothing comes back to adopt. Every other client learns
/// the new title from the `SessionUpdated` patch the endpoint broadcasts, which
/// carries the STORED value.
pub(crate) async fn set_title<C: GatewayJsonClient + Sync>(
    client: &C,
    session_id: String,
    title: String,
) -> Result<(), String> {
    validate_path_segment(&session_id, "session_id")?;
    let body = serde_json::to_vec(&SetTitleRequest { title: &title })
        .map_err(|e| format!("encode set title request: {e}"))?;
    let path = format!("{PATH_CHAT_SESSIONS}/{session_id}/title");
    client.put_empty(&path, body).await
}

/// Pin/unpin a **cron group** (`docs/cron-groups.md`) — keyed by the JOB, not a
/// session: the group is a view over the job's fires, and the job is the only
/// object that can hold the bit. `PUT /v1/cron/{id}/pin`. The first cron-job
/// mutation this client can make, so it is also the first `/v1/cron` path it
/// touches at all.
pub(crate) async fn set_cron_pinned<C: GatewayJsonClient + Sync>(
    client: &C,
    job_id: String,
    pinned: bool,
) -> Result<(), String> {
    validate_path_segment(&job_id, "job_id")?;
    let body = serde_json::to_vec(&SetPinnedRequest { pinned })
        .map_err(|e| format!("encode set cron pin request: {e}"))?;
    let path = format!("{PATH_CRON}/{job_id}/pin");
    client.put_empty(&path, body).await
}

pub(crate) async fn hide_session<C: GatewayJsonClient + Sync>(
    client: &C,
    session_id: String,
) -> Result<(), String> {
    validate_path_segment(&session_id, "session_id")?;
    let path = format!("{PATH_CHAT_SESSIONS}/{session_id}");
    client.delete_empty(&path).await
}

/// Hide every named session in ONE round-trip — like [`hide_session`], each row
/// survives on the server (see the gateway's chat module docstring); only the
/// list loses it.
///
/// Behind the cron group's delete swipe: "delete the group" means "clear its
/// execution records", and the group is a VIEW, so there is nothing to delete
/// but its fires — one hide each. `POST /v1/chat/sessions/hide`.
pub(crate) async fn hide_many<C: GatewayJsonClient + Sync>(
    client: &C,
    session_ids: Vec<String>,
) -> Result<(), String> {
    if session_ids.is_empty() {
        return Ok(());
    }
    let body = serde_json::to_vec(&HideManyRequest { session_ids })
        .map_err(|e| format!("encode batch hide request: {e}"))?;
    let path = format!("{PATH_CHAT_SESSIONS}/hide");
    client.post_empty(&path, body).await
}

/// Advance the session's chat-list read cursor (max-wins server-side) — the
/// highest ordinal the viewer has read. Clears the unread badge on the next
/// list pull. `PUT /v1/chat/sessions/{id}/read`.
pub(crate) async fn mark_read<C: GatewayJsonClient + Sync>(
    client: &C,
    session_id: String,
    ordinal: i64,
) -> Result<(), String> {
    validate_path_segment(&session_id, "session_id")?;
    let body = serde_json::to_vec(&MarkReadRequest { ordinal })
        .map_err(|e| format!("encode mark-read request: {e}"))?;
    let path = format!("{PATH_CHAT_SESSIONS}/{session_id}/read");
    client.put_empty(&path, body).await
}

/// Mark every named session fully read in ONE round-trip — the gateway resolves
/// each session's own tail ordinal, which a chat-list client does not have.
///
/// Behind the cron group's "mark all read" swipe: a `*/30` job accrues 48 fires
/// a day, and looping [`mark_read`] over them would be 48 round-trips through
/// the relay tunnel. `POST /v1/chat/sessions/read`.
pub(crate) async fn mark_many_read<C: GatewayJsonClient + Sync>(
    client: &C,
    session_ids: Vec<String>,
) -> Result<(), String> {
    if session_ids.is_empty() {
        return Ok(());
    }
    let body = serde_json::to_vec(&MarkManyReadRequest { session_ids })
        .map_err(|e| format!("encode batch mark-read request: {e}"))?;
    let path = format!("{PATH_CHAT_SESSIONS}/read");
    client.post_empty(&path, body).await
}

pub(crate) async fn fetch_history_page<C: GatewayJsonClient + Sync>(
    client: &C,
    session_id: String,
    before_ordinal: Option<i64>,
    limit: Option<u32>,
) -> Result<String, String> {
    history_page(
        client,
        PATH_CHAT_SESSIONS,
        session_id,
        before_ordinal,
        limit,
    )
    .await
}

/// A child's backward transcript page (`GET /v1/chat/subagents/{id}`). The
/// route answers the same DTO as its owner-session twin, and the frame feeds
/// the same transcript bundle — the read-only page differs by which door it
/// came through, nothing else.
pub(crate) async fn fetch_subagent_history_page<C: GatewayJsonClient + Sync>(
    client: &C,
    session_id: String,
    before_ordinal: Option<i64>,
    limit: Option<u32>,
) -> Result<String, String> {
    history_page(
        client,
        PATH_CHAT_SUBAGENTS,
        session_id,
        before_ordinal,
        limit,
    )
    .await
}

async fn history_page<C: GatewayJsonClient + Sync>(
    client: &C,
    base: &str,
    session_id: String,
    before_ordinal: Option<i64>,
    limit: Option<u32>,
) -> Result<String, String> {
    validate_path_segment(&session_id, "session_id")?;
    let path = format!("{base}/{}", percent_encode(&session_id));
    history_page_at(client, path, before_ordinal, limit).await
}

/// The page fetch itself, against a path the caller has already built.
///
/// Split out because a card's run transcript is not addressed by a session
/// id at all — it hangs off the board, the card and the attempt
/// (`…/issues/{n}/runs/{a}/transcript`) — while answering the identical DTO
/// and feeding the identical webview frame.
async fn history_page_at<C: GatewayJsonClient + Sync>(
    client: &C,
    mut path: String,
    before_ordinal: Option<i64>,
    limit: Option<u32>,
) -> Result<String, String> {
    let mut first_query = true;
    if let Some(before) = before_ordinal {
        append_query(&mut path, &mut first_query, "before_ordinal", before);
    }
    if let Some(limit) = limit {
        append_query(&mut path, &mut first_query, "limit", limit);
    }
    let detail: ChatSessionDetail = client.get_json(&path).await?;
    let page = HistoryPageFrame {
        kind: "history_page",
        rows: detail.transcript,
        oldest_ordinal: detail.oldest_ordinal,
        newest_ordinal: detail.newest_ordinal,
        has_more: detail.has_more,
    };
    serde_json::to_string(&page).map_err(|e| format!("encode history page: {e}"))
}

/// The one forward-recovery pull (sync-v2): fetch the difference after
/// `since_ordinal` (or the newest-page baseline when `None`) and synthesize a
/// `sync_page` frame for the web transcript bridge, rows verbatim.
pub(crate) async fn fetch_sync<C: GatewayJsonClient + Sync>(
    client: &C,
    session_id: String,
    since_ordinal: Option<i64>,
    limit: u32,
) -> Result<String, String> {
    sync_page(client, PATH_CHAT_SESSIONS, session_id, since_ordinal, limit).await
}

/// The same forward-recovery pull against a child (`GET
/// /v1/chat/subagents/{id}/sync`) — what the read-only page polls while the
/// child it is showing has not ended, cheap because it is a cursor difference.
pub(crate) async fn fetch_subagent_sync<C: GatewayJsonClient + Sync>(
    client: &C,
    session_id: String,
    since_ordinal: Option<i64>,
    limit: u32,
) -> Result<String, String> {
    sync_page(
        client,
        PATH_CHAT_SUBAGENTS,
        session_id,
        since_ordinal,
        limit,
    )
    .await
}

async fn sync_page<C: GatewayJsonClient + Sync>(
    client: &C,
    base: &str,
    session_id: String,
    since_ordinal: Option<i64>,
    limit: u32,
) -> Result<String, String> {
    validate_path_segment(&session_id, "session_id")?;
    let mut path = format!("{base}/{}/sync", percent_encode(&session_id));
    let mut first_query = true;
    if let Some(since) = since_ordinal {
        append_query(&mut path, &mut first_query, "since_ordinal", since);
    }
    append_query(&mut path, &mut first_query, "limit", limit);
    let response: ChatSyncResponse = client.get_json(&path).await?;
    let frame = SyncPageFrame {
        kind: "sync_page",
        rows: response.rows,
        since_ordinal,
        next_cursor: response.next_cursor,
        rebased: response.rebased,
        oldest_ordinal: response.oldest_ordinal,
        has_more_older: response.has_more_older,
        compaction_points: response.compaction_points,
    };
    serde_json::to_string(&frame).map_err(|e| format!("encode sync page: {e}"))
}

/// A session's direct subagent children, ascending
/// (`GET /v1/chat/sessions/{id}/subagents`).
///
/// One bounded listing, never a page: the route caps what it returns and
/// reports how many older children the cap dropped, so there is no cursor to
/// follow. `session_id` is a parent — an owner conversation, or a child being
/// drilled into for its own children.
pub(crate) async fn list_subagents<C: GatewayJsonClient + Sync>(
    client: &C,
    session_id: String,
    before: Option<SubagentCursor>,
) -> Result<ChatSubagentList, String> {
    validate_path_segment(&session_id, "session_id")?;
    let mut path = format!(
        "{PATH_CHAT_SESSIONS}/{}/subagents",
        percent_encode(&session_id)
    );
    // Both halves or neither: the gateway ignores a partial cursor, which
    // would silently page from somewhere the caller did not mean.
    if let Some(cursor) = before {
        validate_path_segment(&cursor.session_id, "before_id")?;
        path.push_str(&format!(
            "?before_created_at={}&before_id={}",
            percent_encode(&cursor.created_at),
            percent_encode(&cursor.session_id)
        ));
    }
    let list: WireSubagentList = client.get_json(&path).await?;
    Ok(ChatSubagentList {
        has_more_older: list.has_more_older,
        items: list
            .items
            .into_iter()
            .map(|child| ChatSubagentSummary {
                session_id: child.session_id,
                subagent_type: child.subagent_type,
                backend: child.backend,
                task: child.task,
                status: match child.status {
                    WireSubagentStatus::Pending => ChatSubagentStatus::Pending,
                    WireSubagentStatus::Running => ChatSubagentStatus::Running,
                    WireSubagentStatus::Completed => ChatSubagentStatus::Completed,
                    WireSubagentStatus::Failed => ChatSubagentStatus::Failed,
                    WireSubagentStatus::Cancelled => ChatSubagentStatus::Cancelled,
                    WireSubagentStatus::Unknown => ChatSubagentStatus::Unknown,
                },
                created_at: child.created_at,
                started_at: child.started_at,
                ended_at: child.ended_at,
            })
            .collect(),
    })
}

/// Per-send durability point lookup: does a persisted row carry this
/// `platform_msg_id`? Consumed natively by the outbox (never the webview).
pub(crate) async fn lookup_message<C: GatewayJsonClient + Sync>(
    client: &C,
    session_id: String,
    platform_msg_id: &str,
) -> Result<ChatMessageLookupResponse, String> {
    validate_path_segment(&session_id, "session_id")?;
    if platform_msg_id.trim().is_empty() {
        return Err("invalid platform_msg_id".to_string());
    }
    let path = format!(
        "{PATH_CHAT_SESSIONS}/{session_id}/messages?platform_msg_id={}",
        percent_encode(platform_msg_id)
    );
    client.get_json(&path).await
}

/// Full-text search over the owner's transcripts (`GET /v1/chat/search`).
///
/// Only `q` is sent, so the gateway's defaults stand: no hidden sessions (the
/// user asked to lose those), no archived ones, and no cron workspaces. That is
/// byte-for-byte the scope app/web's search panel asks for — search is one
/// protocol implemented twice, and the two must not drift.
///
/// `limit` is deliberately not sent either: the server default (20
/// conversations) is what fills a result list, and the scan window that decides
/// `truncated` is the server's regardless.
pub(crate) async fn search_messages<C: GatewayJsonClient + Sync>(
    client: &C,
    query: &str,
) -> Result<ChatSearchResults, String> {
    let path = format!("{PATH_CHAT_SEARCH}?q={}", percent_encode(query));
    let wire: WireSearchResults = client.get_json(&path).await?;
    Ok(ChatSearchResults {
        truncated: wire.truncated,
        groups: wire
            .groups
            .into_iter()
            .map(|group| ChatSearchGroup {
                session_id: group.session_id,
                session_title: group.session_title,
                total_hits: group.total_hits,
                hits: group
                    .hits
                    .into_iter()
                    .map(|hit| ChatSearchHit {
                        ordinal: hit.ordinal,
                        role: hit.role,
                        text: hit.text,
                        created_at: hit.created_at,
                        superseded_by: hit.superseded_by,
                    })
                    .collect(),
            })
            .collect(),
    })
}

/// Percent-encode a URL value (everything outside RFC 3986 unreserved) — the
/// unreserved set is literal in a query value and in a path segment alike, so
/// both positions ride this. `platform_msg_id`s are native-minted UUIDs today,
/// but a retry payload round-trips through the webview, and a subagent child
/// id is whatever the gateway's listing said — encode defensively.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The gateway's configured LLM entries + the current `default-llm` name
/// (`GET /v1/llm/models`) — the chat header model picker's catalog. Global,
/// not per-session; the client caches it per app run.
pub(crate) async fn list_llm_models<C: GatewayJsonClient + Sync>(
    client: &C,
) -> Result<LlmModelCatalog, String> {
    let list: LlmModelsList = client.get_json(PATH_LLM_MODELS).await?;
    Ok(LlmModelCatalog {
        default_name: list.default_name,
        items: list
            .items
            .into_iter()
            .map(|m| LlmModelInfo {
                name: m.name,
                provider: m.provider,
                model: m.model,
                model_candidates: m.model_list.into_iter().map(|s| s.model).collect(),
                reasoning_effort: m.reasoning_effort,
                available_efforts: m.available_efforts,
            })
            .collect(),
    })
}

/// The session's model pin: the `baybo.json` entry name (`last_llm`) its turns
/// resolve against and the chosen model within it (`last_model`), either
/// `None`. Read off the session detail with `limit=1` — the pin rides the same
/// DTO as the transcript page, and one throwaway row is the smallest page the
/// route serves.
pub(crate) async fn fetch_session_model<C: GatewayJsonClient + Sync>(
    client: &C,
    session_id: String,
) -> Result<SessionModelPin, String> {
    validate_path_segment(&session_id, "session_id")?;
    let meta: SessionModelMeta = client
        .get_json(&format!("{PATH_CHAT_SESSIONS}/{session_id}?limit=1"))
        .await?;
    Ok(SessionModelPin {
        llm: meta.last_llm,
        model: meta.last_model,
        effort: meta.last_effort,
    })
}

/// Pin the session to LLM entry `llm` and model `model` within it — clear the
/// entry (`llm = None` → follow `default-llm`) and/or fall back to the entry's
/// default model (`model = None`). `PUT /v1/chat/sessions/{id}/model`; the
/// gateway persists the pin first and re-pins any live actor, so it applies
/// from the session's next turn. The response body (the echoed pin) is
/// deliberately discarded — the route validates the pin, so a 200 means it took.
pub(crate) async fn set_session_model<C: GatewayJsonClient + Sync>(
    client: &C,
    session_id: String,
    llm: Option<String>,
    model: Option<String>,
    effort: Option<String>,
) -> Result<(), String> {
    validate_path_segment(&session_id, "session_id")?;
    let body = serde_json::to_vec(&SetSessionModelRequest {
        llm: llm.as_deref(),
        model: model.as_deref(),
        reasoning_effort: effort.as_deref(),
    })
    .map_err(|e| format!("encode set session model request: {e}"))?;
    let path = format!("{PATH_CHAT_SESSIONS}/{session_id}/model");
    client.put_empty(&path, body).await
}

pub(crate) async fn update_apns_token<C: GatewayJsonClient + Sync>(
    client: &C,
    apns_token: &str,
    apns_env: &str,
) -> Result<(), String> {
    let body = serde_json::to_vec(&UpdateApnsTokenRequest {
        apns_token,
        apns_env,
    })
    .map_err(|e| format!("encode APNs token update: {e}"))?;
    client.post_empty(PATH_MOBILE_APNS_TOKEN, body).await
}

// ─────────────────────────── projects (kanban boards) ───────────────────────
//
// Every enum decodes tolerantly (`#[serde(other)] Unknown`) for the reason
// `WireSubagentStatus` does: a gateway that grows a status must cost one card
// its word, not fail the whole board's decode. Encoding refuses `Unknown` —
// asking the server to move a card into a column this build cannot name is a
// request nobody can honour.

/// The gateway's `ListResponse<T>` envelope. Generic here rather than one
/// wrapper per route, as the older surfaces have: this path space answers
/// six different lists in the same shape, and `next_cursor` is unused by
/// every one of them (the feed pages by `before_ms` instead).
#[derive(Deserialize)]
struct WireList<T> {
    #[serde(default = "Vec::new")]
    items: Vec<T>,
}

/// Absent means "leave it", explicit `null` means "clear it". The gateway
/// reads both halves through its `double_option`; this is the serialize side.
fn patch_field(patch: &crate::api::StringPatch) -> Option<Option<&str>> {
    match patch {
        crate::api::StringPatch::Keep => None,
        crate::api::StringPatch::Clear => Some(None),
        crate::api::StringPatch::Set { value } => Some(Some(value.as_str())),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireIssueStatus {
    Backlog,
    Todo,
    InProgress,
    Review,
    Done,
    #[serde(other)]
    Unknown,
}

impl From<WireIssueStatus> for IssueStatus {
    fn from(s: WireIssueStatus) -> Self {
        match s {
            WireIssueStatus::Backlog => IssueStatus::Backlog,
            WireIssueStatus::Todo => IssueStatus::Todo,
            WireIssueStatus::InProgress => IssueStatus::InProgress,
            WireIssueStatus::Review => IssueStatus::Review,
            WireIssueStatus::Done => IssueStatus::Done,
            WireIssueStatus::Unknown => IssueStatus::Unknown,
        }
    }
}

fn status_wire(status: IssueStatus) -> Result<&'static str, String> {
    match status {
        IssueStatus::Backlog => Ok("backlog"),
        IssueStatus::Todo => Ok("todo"),
        IssueStatus::InProgress => Ok("in_progress"),
        IssueStatus::Review => Ok("review"),
        IssueStatus::Done => Ok("done"),
        IssueStatus::Unknown => Err("cannot move a card to an unknown status".to_string()),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireIssuePriority {
    Urgent,
    High,
    Medium,
    Low,
    None,
    #[serde(other)]
    Unknown,
}

impl From<WireIssuePriority> for IssuePriority {
    fn from(p: WireIssuePriority) -> Self {
        match p {
            WireIssuePriority::Urgent => IssuePriority::Urgent,
            WireIssuePriority::High => IssuePriority::High,
            WireIssuePriority::Medium => IssuePriority::Medium,
            WireIssuePriority::Low => IssuePriority::Low,
            WireIssuePriority::None => IssuePriority::None,
            WireIssuePriority::Unknown => IssuePriority::Unknown,
        }
    }
}

fn priority_wire(priority: IssuePriority) -> Result<&'static str, String> {
    match priority {
        IssuePriority::Urgent => Ok("urgent"),
        IssuePriority::High => Ok("high"),
        IssuePriority::Medium => Ok("medium"),
        IssuePriority::Low => Ok("low"),
        IssuePriority::None => Ok("none"),
        IssuePriority::Unknown => Err("cannot set an unknown priority".to_string()),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireRunStatus {
    Held,
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
    #[serde(other)]
    Unknown,
}

impl From<WireRunStatus> for RunStatus {
    fn from(s: WireRunStatus) -> Self {
        match s {
            WireRunStatus::Held => RunStatus::Held,
            WireRunStatus::Queued => RunStatus::Queued,
            WireRunStatus::Running => RunStatus::Running,
            WireRunStatus::Done => RunStatus::Done,
            WireRunStatus::Failed => RunStatus::Failed,
            WireRunStatus::Cancelled => RunStatus::Cancelled,
            WireRunStatus::Unknown => RunStatus::Unknown,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireRunTrigger {
    Started,
    Assigned,
    Retry,
    Comment,
    Promoted,
    Triage,
    StageBarrier,
    Review,
    Stalled,
    Blocked,
    Grooming,
    BoardIdle,
    #[serde(other)]
    Unknown,
}

impl From<WireRunTrigger> for RunTrigger {
    fn from(t: WireRunTrigger) -> Self {
        match t {
            WireRunTrigger::Started => RunTrigger::Started,
            WireRunTrigger::Assigned => RunTrigger::Assigned,
            WireRunTrigger::Retry => RunTrigger::Retry,
            WireRunTrigger::Comment => RunTrigger::Comment,
            WireRunTrigger::Promoted => RunTrigger::Promoted,
            WireRunTrigger::Triage => RunTrigger::Triage,
            WireRunTrigger::StageBarrier => RunTrigger::StageBarrier,
            WireRunTrigger::Review => RunTrigger::Review,
            WireRunTrigger::Stalled => RunTrigger::Stalled,
            WireRunTrigger::Blocked => RunTrigger::Blocked,
            WireRunTrigger::Grooming => RunTrigger::Grooming,
            WireRunTrigger::BoardIdle => RunTrigger::BoardIdle,
            WireRunTrigger::Unknown => RunTrigger::Unknown,
        }
    }
}

#[derive(Deserialize)]
struct WireProject {
    id: String,
    name: String,
    description: String,
    workdir: String,
    #[serde(default)]
    daily_budget_micros: Option<i64>,
    #[serde(default)]
    daily_budget_tokens: Option<i64>,
    max_parallel_issue_runs: i64,
    #[serde(default)]
    agents_may_merge: bool,
    #[serde(default)]
    archived_at_ms: Option<i64>,
    created_at_ms: i64,
    updated_at_ms: i64,
}

impl From<WireProject> for ProjectInfo {
    fn from(p: WireProject) -> Self {
        ProjectInfo {
            id: p.id,
            name: p.name,
            description: p.description,
            workdir: p.workdir,
            daily_budget_micros: p.daily_budget_micros,
            daily_budget_tokens: p.daily_budget_tokens,
            max_parallel_issue_runs: p.max_parallel_issue_runs,
            agents_may_merge: p.agents_may_merge,
            archived_at_ms: p.archived_at_ms,
            created_at_ms: p.created_at_ms,
            updated_at_ms: p.updated_at_ms,
        }
    }
}

#[derive(Deserialize)]
struct WireIssueAttachment {
    blob_id: String,
    mime_type: String,
    size: u32,
    #[serde(default)]
    filename: Option<String>,
}

#[derive(Deserialize)]
struct WireSubIssues {
    done: i64,
    total: i64,
}

#[derive(Deserialize)]
struct WireIssue {
    number: i64,
    project_id: String,
    title: String,
    description: String,
    #[serde(default)]
    attachments: Vec<WireIssueAttachment>,
    status: WireIssueStatus,
    priority: WireIssuePriority,
    #[serde(default)]
    assignee: Option<String>,
    position: i64,
    pinned: bool,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    blocked_reason: Option<String>,
    #[serde(default)]
    parent: Option<i64>,
    #[serde(default)]
    filed_from: Option<i64>,
    stage: i64,
    #[serde(default)]
    sub_issues: Option<WireSubIssues>,
    unread: i64,
    last_run_failed: bool,
    // Predates this client on a gateway that has not shipped the field yet;
    // absent reads as "nothing waiting", the direction that shows no badge
    // rather than a badge with nothing behind it.
    #[serde(default)]
    approval_pending: bool,
    #[serde(default)]
    opened_by_agent: bool,
    #[serde(default)]
    cancelled_at_ms: Option<i64>,
    created_at_ms: i64,
    updated_at_ms: i64,
}

impl From<WireIssue> for IssueInfo {
    fn from(i: WireIssue) -> Self {
        IssueInfo {
            number: i.number,
            project_id: i.project_id,
            title: i.title,
            description: i.description,
            attachments: i
                .attachments
                .into_iter()
                .map(|a| IssueAttachmentInfo {
                    blob_id: a.blob_id,
                    mime_type: a.mime_type,
                    size: a.size,
                    filename: a.filename,
                })
                .collect(),
            status: i.status.into(),
            priority: i.priority.into(),
            assignee: i.assignee,
            position: i.position,
            pinned: i.pinned,
            branch: i.branch,
            blocked_reason: i.blocked_reason,
            parent: i.parent,
            filed_from: i.filed_from,
            stage: i.stage,
            sub_issues: i.sub_issues.map(|s| SubIssueProgress {
                done: s.done,
                total: s.total,
            }),
            unread: i.unread,
            last_run_failed: i.last_run_failed,
            approval_pending: i.approval_pending,
            opened_by_agent: i.opened_by_agent,
            cancelled_at_ms: i.cancelled_at_ms,
            created_at_ms: i.created_at_ms,
            updated_at_ms: i.updated_at_ms,
        }
    }
}

#[derive(Deserialize)]
struct WireIssueRun {
    number: i64,
    attempt: i64,
    agent_id: String,
    status: WireRunStatus,
    trigger: WireRunTrigger,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    error: Option<String>,
    created_at_ms: i64,
    #[serde(default)]
    started_at_ms: Option<i64>,
    #[serde(default)]
    settled_at_ms: Option<i64>,
    #[serde(default)]
    cost_micros: Option<i64>,
    #[serde(default)]
    input_tokens: Option<i64>,
    #[serde(default)]
    output_tokens: Option<i64>,
}

impl From<WireIssueRun> for IssueRunInfo {
    fn from(r: WireIssueRun) -> Self {
        IssueRunInfo {
            number: r.number,
            attempt: r.attempt,
            agent_id: r.agent_id,
            status: r.status.into(),
            trigger: r.trigger.into(),
            session_id: r.session_id,
            error: r.error,
            created_at_ms: r.created_at_ms,
            started_at_ms: r.started_at_ms,
            settled_at_ms: r.settled_at_ms,
            cost_micros: r.cost_micros,
            input_tokens: r.input_tokens,
            output_tokens: r.output_tokens,
        }
    }
}

#[derive(Deserialize)]
struct WireIssueRunLog {
    #[serde(default)]
    items: Vec<WireIssueRun>,
    #[serde(default)]
    total_cost_micros: i64,
    #[serde(default)]
    total_input_tokens: i64,
    #[serde(default)]
    total_output_tokens: i64,
}

#[derive(Deserialize)]
struct WireHiredBy {
    id: String,
    handle: String,
}

#[derive(Deserialize)]
struct WireTeamMember {
    id: String,
    handle: String,
    name: String,
    description: String,
    #[serde(default)]
    avatar_blob_id: Option<String>,
    framework: String,
    #[serde(default)]
    llm: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    lead: bool,
    #[serde(default)]
    hired_by: Option<WireHiredBy>,
    created_at_ms: i64,
}

impl From<WireTeamMember> for TeamMemberInfo {
    fn from(m: WireTeamMember) -> Self {
        TeamMemberInfo {
            id: m.id,
            handle: m.handle,
            name: m.name,
            description: m.description,
            avatar_blob_id: m.avatar_blob_id,
            framework: m.framework,
            llm: m.llm,
            model: m.model,
            reasoning_effort: m.reasoning_effort,
            lead: m.lead,
            hired_by: m.hired_by.map(|h| HiredBy {
                id: h.id,
                handle: h.handle,
            }),
            created_at_ms: m.created_at_ms,
        }
    }
}

#[derive(Deserialize)]
struct WireAttention {
    project_id: String,
    name: String,
    approvals: u32,
    failed: u32,
    unread: u32,
}

#[derive(Deserialize)]
struct WireActivity {
    project_id: String,
    working: u32,
    burn_micros: i64,
    burn_tokens: i64,
}

#[derive(Serialize)]
struct NewProjectRequest<'a> {
    name: &'a str,
    description: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    workdir: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    daily_budget_micros: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    daily_budget_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_parallel_issue_runs: Option<i64>,
}

/// A full replace — every knob is written as given, so nothing here is
/// skipped when absent. `null` clears a ceiling, which is what "no limit"
/// is on the wire.
#[derive(Serialize)]
struct UpdateProjectRequest<'a> {
    name: &'a str,
    description: &'a str,
    daily_budget_micros: Option<i64>,
    daily_budget_tokens: Option<i64>,
    max_parallel_issue_runs: Option<i64>,
    /// Never omitted. `#[serde(default)]` on the gateway's side reads a
    /// missing `agents_may_merge` as `false`, so leaving it out of a
    /// full-replace body is not "leave it alone" — it is "turn it off".
    agents_may_merge: bool,
}

#[derive(Serialize)]
struct SetProjectArchivedRequest {
    archived: bool,
}

#[derive(Serialize)]
struct AttachmentRequest<'a> {
    blob_id: &'a str,
    /// What to call the file. The gateway reads mime and size off the blob
    /// itself, but nothing there knows what the user picked it as — omit this
    /// and every file card on a card page prints an inferred name.
    #[serde(skip_serializing_if = "Option::is_none")]
    filename: Option<&'a str>,
}

#[derive(Serialize)]
struct NewIssueRequest<'a> {
    title: &'a str,
    description: &'a str,
    attachments: Vec<AttachmentRequest<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    priority: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    assignee: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stage: Option<i64>,
}

/// Sparse: an omitted key leaves the field alone. `assignee` and
/// `blocked_reason` are doubly optional — `Some(None)` serializes as an
/// explicit `null`, which is how a card is unassigned or a block lifted.
#[derive(Serialize)]
struct UpdateIssueRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attachments: Option<Vec<AttachmentRequest<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    priority: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    assignee: Option<Option<&'a str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    blocked_reason: Option<Option<&'a str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cancelled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stage: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pinned: Option<bool>,
}

#[derive(Serialize)]
struct MoveIssueRequest<'a> {
    status: &'a str,
    /// Every number in the destination column, in its final order, this card
    /// among them — the gateway renumbers the column in one transaction.
    ordered_numbers: Vec<i64>,
}

#[derive(Serialize)]
struct NewCommentRequest<'a> {
    text: &'a str,
    attachments: Vec<AttachmentRequest<'a>>,
}

#[derive(Serialize)]
struct ResolveIssueApprovalRequest {
    decision: &'static str,
}

fn project_path(project: &str) -> Result<String, String> {
    validate_path_segment(project, "project_id")?;
    Ok(format!("{PATH_PROJECTS}/{}", percent_encode(project)))
}

fn issue_path(project: &str, number: i64) -> Result<String, String> {
    Ok(format!("{}/issues/{number}", project_path(project)?))
}

pub(crate) async fn list_projects<C: GatewayJsonClient + Sync>(
    client: &C,
    include_archived: bool,
) -> Result<Vec<ProjectInfo>, String> {
    let path = if include_archived {
        format!("{PATH_PROJECTS}?include_archived=true")
    } else {
        PATH_PROJECTS.to_string()
    };
    let response: WireList<WireProject> = client.get_json(&path).await?;
    Ok(response.items.into_iter().map(ProjectInfo::from).collect())
}

pub(crate) async fn fetch_project<C: GatewayJsonClient + Sync>(
    client: &C,
    project: String,
) -> Result<ProjectInfo, String> {
    let wire: WireProject = client.get_json(&project_path(&project)?).await?;
    Ok(wire.into())
}

pub(crate) async fn create_project<C: GatewayJsonClient + Sync>(
    client: &C,
    new: crate::api::NewProject,
) -> Result<ProjectInfo, String> {
    let body = serde_json::to_vec(&NewProjectRequest {
        name: &new.name,
        description: &new.description,
        workdir: new.workdir.as_deref(),
        daily_budget_micros: new.daily_budget_micros,
        daily_budget_tokens: new.daily_budget_tokens,
        max_parallel_issue_runs: new.max_parallel_issue_runs,
    })
    .map_err(|e| format!("encode new project: {e}"))?;
    let wire: WireProject = client.post_json(PATH_PROJECTS, body).await?;
    Ok(wire.into())
}

/// Write the board's knobs. A **full replace**: an omitted ceiling is a
/// cleared ceiling, so callers read the current settings, change one and
/// send them all.
///
/// Returns nothing on purpose. The route answers with the row, but a full
/// replace is a body the caller authored field by field — there is nothing
/// in the response it does not already hold, and the board refetches after
/// a write regardless (a ceiling change releases held runs).
pub(crate) async fn update_project<C: GatewayJsonClient + Sync>(
    client: &C,
    project: String,
    settings: crate::api::ProjectSettings,
) -> Result<(), String> {
    let path = project_path(&project)?;
    let body = serde_json::to_vec(&UpdateProjectRequest {
        name: &settings.name,
        description: &settings.description,
        daily_budget_micros: settings.daily_budget_micros,
        daily_budget_tokens: settings.daily_budget_tokens,
        max_parallel_issue_runs: settings.max_parallel_issue_runs,
        agents_may_merge: settings.agents_may_merge,
    })
    .map_err(|e| format!("encode project settings: {e}"))?;
    client.put_empty(&path, body).await
}

pub(crate) async fn set_project_archived<C: GatewayJsonClient + Sync>(
    client: &C,
    project: String,
    archived: bool,
) -> Result<ProjectInfo, String> {
    let path = format!("{}/archive", project_path(&project)?);
    let body = serde_json::to_vec(&SetProjectArchivedRequest { archived })
        .map_err(|e| format!("encode archive request: {e}"))?;
    let wire: WireProject = client.post_json(&path, body).await?;
    Ok(wire.into())
}

pub(crate) async fn list_project_issues<C: GatewayJsonClient + Sync>(
    client: &C,
    project: String,
) -> Result<Vec<IssueInfo>, String> {
    let path = format!("{}/issues", project_path(&project)?);
    let response: WireList<WireIssue> = client.get_json(&path).await?;
    Ok(response.items.into_iter().map(IssueInfo::from).collect())
}

pub(crate) async fn fetch_issue<C: GatewayJsonClient + Sync>(
    client: &C,
    project: String,
    number: i64,
) -> Result<IssueInfo, String> {
    let wire: WireIssue = client.get_json(&issue_path(&project, number)?).await?;
    Ok(wire.into())
}

pub(crate) async fn create_issue<C: GatewayJsonClient + Sync>(
    client: &C,
    project: String,
    new: crate::api::NewIssue,
) -> Result<IssueInfo, String> {
    let path = format!("{}/issues", project_path(&project)?);
    let status = new.status.map(status_wire).transpose()?;
    let priority = new.priority.map(priority_wire).transpose()?;
    let body = serde_json::to_vec(&NewIssueRequest {
        title: &new.title,
        description: &new.description,
        attachments: new
            .attachments
            .iter()
            .map(|blob_id| AttachmentRequest {
                blob_id,
                filename: None,
            })
            .collect(),
        status,
        priority,
        assignee: new.assignee.as_deref(),
        parent: new.parent,
        stage: new.stage,
    })
    .map_err(|e| format!("encode new issue: {e}"))?;
    let wire: WireIssue = client.post_json(&path, body).await?;
    Ok(wire.into())
}

pub(crate) async fn patch_issue<C: GatewayJsonClient + Sync>(
    client: &C,
    project: String,
    number: i64,
    patch: crate::api::IssuePatch,
) -> Result<IssueInfo, String> {
    let path = issue_path(&project, number)?;
    let priority = patch.priority.map(priority_wire).transpose()?;
    let body = serde_json::to_vec(&UpdateIssueRequest {
        title: patch.title.as_deref(),
        description: patch.description.as_deref(),
        attachments: patch.attachments.as_ref().map(|ids| {
            ids.iter()
                .map(|blob_id| AttachmentRequest {
                    blob_id,
                    filename: None,
                })
                .collect()
        }),
        priority,
        assignee: patch_field(&patch.assignee),
        blocked_reason: patch_field(&patch.blocked_reason),
        cancelled: patch.cancelled,
        parent: patch.parent,
        stage: patch.stage,
        pinned: patch.pinned,
    })
    .map_err(|e| format!("encode issue patch: {e}"))?;
    let wire: WireIssue = client.patch_json(&path, body).await?;
    Ok(wire.into())
}

pub(crate) async fn move_issue<C: GatewayJsonClient + Sync>(
    client: &C,
    project: String,
    number: i64,
    status: IssueStatus,
    ordered_numbers: Vec<i64>,
) -> Result<IssueInfo, String> {
    let path = format!("{}/move", issue_path(&project, number)?);
    let body = serde_json::to_vec(&MoveIssueRequest {
        status: status_wire(status)?,
        ordered_numbers,
    })
    .map_err(|e| format!("encode issue move: {e}"))?;
    let wire: WireIssue = client.post_json(&path, body).await?;
    Ok(wire.into())
}

/// Unsettled runs across the whole board — what the card faces read their
/// working / queued / held word from. These carry **no cost fields**; the
/// per-card log does.
pub(crate) async fn list_active_runs<C: GatewayJsonClient + Sync>(
    client: &C,
    project: String,
) -> Result<Vec<IssueRunInfo>, String> {
    let path = format!("{}/runs", project_path(&project)?);
    let response: WireList<WireIssueRun> = client.get_json(&path).await?;
    Ok(response.items.into_iter().map(IssueRunInfo::from).collect())
}

pub(crate) async fn list_issue_runs<C: GatewayJsonClient + Sync>(
    client: &C,
    project: String,
    number: i64,
) -> Result<crate::api::IssueRunLog, String> {
    let path = format!("{}/runs", issue_path(&project, number)?);
    let log: WireIssueRunLog = client.get_json(&path).await?;
    Ok(crate::api::IssueRunLog {
        runs: log.items.into_iter().map(IssueRunInfo::from).collect(),
        total_cost_micros: log.total_cost_micros,
        total_input_tokens: log.total_input_tokens,
        total_output_tokens: log.total_output_tokens,
    })
}

pub(crate) async fn cancel_run<C: GatewayJsonClient + Sync>(
    client: &C,
    project: String,
    number: i64,
) -> Result<(), String> {
    let path = format!("{}/runs/cancel", issue_path(&project, number)?);
    client.post_empty(&path, Vec::new()).await
}

/// The one press that discharges a failed card, and the press that releases a
/// held one — the gateway lets the ceiling through what it can before it
/// refuses, so this is never a no-op on a held run.
pub(crate) async fn retry_run<C: GatewayJsonClient + Sync>(
    client: &C,
    project: String,
    number: i64,
) -> Result<IssueRunInfo, String> {
    let path = format!("{}/runs/retry", issue_path(&project, number)?);
    let wire: WireIssueRun = client.post_json(&path, Vec::new()).await?;
    Ok(wire.into())
}

/// One agent's session on this card, as a backward transcript page. The same
/// `ChatSessionDetail` the chat routes answer, so the transcript webview
/// renders it unchanged; an attempt's page also holds the attempts before it,
/// because one agent's runs on a card share a session.
pub(crate) async fn fetch_run_transcript_page<C: GatewayJsonClient + Sync>(
    client: &C,
    project: String,
    number: i64,
    attempt: i64,
    before_ordinal: Option<i64>,
    limit: Option<u32>,
) -> Result<String, String> {
    let path = format!(
        "{}/runs/{attempt}/transcript",
        issue_path(&project, number)?
    );
    history_page_at(client, path, before_ordinal, limit).await
}

/// A card's timeline, verbatim. Raw JSON rather than a typed record: the
/// entries are a 20-arm tagged union the issue webview renders, and the
/// pieces the native side needs off them (a parked prompt's `call_id`, who
/// blocked the card) are read by one small model rather than by mirroring
/// every arm through UniFFI.
pub(crate) async fn fetch_issue_events<C: GatewayJsonClient + Sync>(
    client: &C,
    project: String,
    number: i64,
) -> Result<String, String> {
    let path = format!("{}/events", issue_path(&project, number)?);
    let events: serde_json::Value = client.get_json(&path).await?;
    serde_json::to_string(&events).map_err(|e| format!("encode issue events: {e}"))
}

/// Post a comment; answers with the timeline entry it became, so the caller
/// appends rather than refetching. Text may be empty when files carry it.
pub(crate) async fn comment_on_issue<C: GatewayJsonClient + Sync>(
    client: &C,
    project: String,
    number: i64,
    text: String,
    attachments: Vec<IssueAttachmentInput>,
) -> Result<String, String> {
    let path = format!("{}/comments", issue_path(&project, number)?);
    let body = serde_json::to_vec(&NewCommentRequest {
        text: &text,
        attachments: attachments
            .iter()
            .map(|a| AttachmentRequest {
                blob_id: &a.blob_id,
                filename: a.filename.as_deref(),
            })
            .collect(),
    })
    .map_err(|e| format!("encode comment: {e}"))?;
    let entry: serde_json::Value = client.post_json(&path, body).await?;
    serde_json::to_string(&entry).map_err(|e| format!("encode comment entry: {e}"))
}

pub(crate) async fn resolve_issue_approval<C: GatewayJsonClient + Sync>(
    client: &C,
    project: String,
    number: i64,
    call_id: String,
    decision: crate::api::IssueApprovalDecision,
) -> Result<(), String> {
    validate_path_segment(&call_id, "call_id")?;
    let path = format!(
        "{}/approvals/{}",
        issue_path(&project, number)?,
        percent_encode(&call_id)
    );
    let body = serde_json::to_vec(&ResolveIssueApprovalRequest {
        decision: match decision {
            crate::api::IssueApprovalDecision::Approve => "approve",
            crate::api::IssueApprovalDecision::Deny => "deny",
        },
    })
    .map_err(|e| format!("encode approval decision: {e}"))?;
    client.post_empty(&path, body).await
}

/// Stamp one card read. The web only sends this once a card's timeline has
/// actually rendered, and the phone follows: a read cursor moved by a screen
/// nobody saw is unread work quietly discarded.
pub(crate) async fn mark_issue_read<C: GatewayJsonClient + Sync>(
    client: &C,
    project: String,
    number: i64,
) -> Result<(), String> {
    let path = format!("{}/read", issue_path(&project, number)?);
    client.post_empty(&path, Vec::new()).await
}

/// Stamp every card on the board read — including the ones a filter is
/// hiding, which is what the press clears.
pub(crate) async fn mark_project_read<C: GatewayJsonClient + Sync>(
    client: &C,
    project: String,
) -> Result<(), String> {
    let path = format!("{}/read", project_path(&project)?);
    client.post_empty(&path, Vec::new()).await
}

/// The board's read-only activity stream, verbatim (see
/// [`fetch_issue_events`] for why it is raw).
pub(crate) async fn fetch_project_feed<C: GatewayJsonClient + Sync>(
    client: &C,
    project: String,
    before_ms: Option<i64>,
    limit: Option<u32>,
) -> Result<String, String> {
    let mut path = format!("{}/feed", project_path(&project)?);
    let mut first_query = true;
    if let Some(before) = before_ms {
        append_query(&mut path, &mut first_query, "before_ms", before);
    }
    if let Some(limit) = limit {
        append_query(&mut path, &mut first_query, "limit", limit);
    }
    let feed: serde_json::Value = client.get_json(&path).await?;
    serde_json::to_string(&feed).map_err(|e| format!("encode project feed: {e}"))
}

pub(crate) async fn list_team<C: GatewayJsonClient + Sync>(
    client: &C,
    project: String,
) -> Result<Vec<TeamMemberInfo>, String> {
    let path = format!("{}/agents", project_path(&project)?);
    let response: WireList<WireTeamMember> = client.get_json(&path).await?;
    Ok(response
        .items
        .into_iter()
        .map(TeamMemberInfo::from)
        .collect())
}

/// Pin (or clear) an agent's LLM, model and thinking level.
///
/// **The whole pin, replaced as one.** Absent means "inherit" at each level,
/// so an empty body clears it entirely rather than leaving two thirds of it
/// pointing at an entry the agent no longer uses. Sending a model without an
/// entry is a 400 — there is no entry to pick it within — which is why the
/// caller must send both or neither.
pub(crate) async fn set_agent_model<C: GatewayJsonClient + Sync>(
    client: &C,
    agent_id: String,
    llm: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
) -> Result<(), String> {
    validate_path_segment(&agent_id, "agent_id")?;
    #[derive(serde::Serialize)]
    struct SetAgentModelRequest {
        llm: Option<String>,
        model: Option<String>,
        reasoning_effort: Option<String>,
    }
    let path = format!("{PATH_AGENTS}/{}/model", percent_encode(&agent_id));
    let body = serde_json::to_vec(&SetAgentModelRequest {
        llm,
        model,
        reasoning_effort,
    })
    .map_err(|e| format!("encode agent model: {e}"))?;
    client.put_empty(&path, body).await
}

/// Give an agent a face, or take it away (`blob_id: None`).
///
/// The blob must already exist and be an image — the gateway stats it and
/// refuses a dangling or non-image reference, because after this the id is a
/// soft reference (no foreign keys).
///
/// **PNG, not SVG.** The generated faces are drawn as SVG by DiceBear, but a
/// native iOS view has no SVG decoder at all: an `image/svg+xml` avatar
/// passes the gateway's `image/*` check and then renders as nothing on every
/// board row. Whoever uploads one rasterises it first.
pub(crate) async fn set_agent_avatar<C: GatewayJsonClient + Sync>(
    client: &C,
    agent_id: String,
    blob_id: Option<String>,
) -> Result<(), String> {
    validate_path_segment(&agent_id, "agent_id")?;
    let path = format!("{PATH_AGENTS}/{}/avatar", percent_encode(&agent_id));
    let body = serde_json::to_vec(&SetAgentAvatarRequest {
        blob_id: blob_id.as_deref(),
    })
    .map_err(|e| format!("encode agent avatar: {e}"))?;
    client.put_empty(&path, body).await
}

#[derive(Serialize)]
struct SetAgentAvatarRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    blob_id: Option<&'a str>,
}

pub(crate) async fn remove_team_member<C: GatewayJsonClient + Sync>(
    client: &C,
    project: String,
    agent_id: String,
) -> Result<(), String> {
    validate_path_segment(&agent_id, "agent_id")?;
    let path = format!(
        "{}/agents/{}",
        project_path(&project)?,
        percent_encode(&agent_id)
    );
    client.delete_empty(&path).await
}

/// Boards with something waiting on the operator. Boards with nothing are
/// **absent**, not zero-valued — a caller summing this must treat a missing
/// board as quiet.
pub(crate) async fn projects_attention<C: GatewayJsonClient + Sync>(
    client: &C,
) -> Result<Vec<ProjectAttention>, String> {
    let path = format!("{PATH_PROJECTS}/attention");
    let response: WireList<WireAttention> = client.get_json(&path).await?;
    Ok(response
        .items
        .into_iter()
        .map(|a| ProjectAttention {
            project_id: a.project_id,
            name: a.name,
            approvals: a.approvals,
            failed: a.failed,
            unread: a.unread,
        })
        .collect())
}

/// What each board has been doing since `since_ms`, which must be the
/// **budget's** day — UTC midnight, not the device's — or the burn figure is
/// measured against a window the ceiling does not use.
pub(crate) async fn projects_activity<C: GatewayJsonClient + Sync>(
    client: &C,
    since_ms: Option<i64>,
) -> Result<Vec<ProjectActivity>, String> {
    let mut path = format!("{PATH_PROJECTS}/activity");
    let mut first_query = true;
    if let Some(since) = since_ms {
        append_query(&mut path, &mut first_query, "since_ms", since);
    }
    let response: WireList<WireActivity> = client.get_json(&path).await?;
    Ok(response
        .items
        .into_iter()
        .map(|a| ProjectActivity {
            project_id: a.project_id,
            working: a.working,
            burn_micros: a.burn_micros,
            burn_tokens: a.burn_tokens,
        })
        .collect())
}

pub(crate) async fn upload_bytes<C: GatewayBlobClient + Sync>(
    client: &C,
    bytes: Vec<u8>,
    mime_type: String,
    deck_card: Option<String>,
) -> Result<String, String> {
    client.upload_blob(bytes, mime_type, deck_card).await
}

pub(crate) async fn upload_file<C: GatewayBlobClient + Sync>(
    client: &C,
    path: String,
    mime_type: String,
    deck_card: Option<String>,
    progress: crate::blob_helper::ProgressSink,
) -> Result<String, String> {
    client
        .upload_blob_file(path, mime_type, deck_card, progress)
        .await
}

pub(crate) async fn download_blob_bytes<C: GatewayBlobClient + Sync>(
    client: &C,
    blob_id: String,
    progress: crate::blob_helper::ProgressSink,
) -> Result<Vec<u8>, String> {
    client.download_blob(blob_id, progress).await
}

fn validate_path_segment(value: &str, name: &str) -> Result<(), String> {
    if value.is_empty() || value.bytes().any(|b| matches!(b, b'/' | b'?' | b'#')) {
        return Err(format!("invalid {name}"));
    }
    Ok(())
}

fn append_query<T: std::fmt::Display>(path: &mut String, first: &mut bool, key: &str, value: T) {
    path.push(if *first { '?' } else { '&' });
    *first = false;
    path.push_str(key);
    path.push('=');
    path.push_str(&value.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    use parking_lot::Mutex;

    /// The frame `kind` strings the web bridge switches on (`web/src/bridge.ts`).
    /// Nothing links the two sides at compile time — a rename here is a silently
    /// ignored frame there, i.e. a transcript that never loads.
    const KIND_SYNC_PAGE: &str = "sync_page";
    const KIND_HISTORY_PAGE: &str = "history_page";

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RecordedCall {
        method: &'static str,
        path: String,
        body: String,
    }

    /// Records what each typed call actually put on the wire and answers with a
    /// canned body. The `GatewayJsonClient` trait is already the seam both legs
    /// meet at, so this exercises the real request-building code.
    struct RecordingClient {
        calls: Mutex<Vec<RecordedCall>>,
        canned: String,
    }

    impl RecordingClient {
        fn new(canned: &str) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                canned: canned.to_string(),
            }
        }

        /// For the calls whose response body is never parsed.
        fn empty() -> Self {
            Self::new("null")
        }

        fn record(&self, method: &'static str, path: &str, body: &[u8]) {
            self.calls.lock().push(RecordedCall {
                method,
                path: path.to_string(),
                body: String::from_utf8_lossy(body).into_owned(),
            });
        }

        fn decode<T: DeserializeOwned>(&self) -> Result<T, String> {
            serde_json::from_str(&self.canned).map_err(|e| format!("decode canned: {e}"))
        }

        fn only_call(&self) -> RecordedCall {
            let calls = self.calls.lock();
            assert_eq!(calls.len(), 1, "expected exactly one call, got {calls:?}");
            calls[0].clone()
        }
    }

    #[allow(clippy::manual_async_fn)]
    impl GatewayJsonClient for RecordingClient {
        fn get_json<'a, T>(
            &'a self,
            path: &'a str,
        ) -> impl Future<Output = Result<T, String>> + Send + 'a
        where
            T: DeserializeOwned + Send + 'static,
        {
            async move {
                self.record("GET", path, b"");
                self.decode()
            }
        }

        fn post_json<'a, T>(
            &'a self,
            path: &'a str,
            body: Vec<u8>,
        ) -> impl Future<Output = Result<T, String>> + Send + 'a
        where
            T: DeserializeOwned + Send + 'static,
        {
            async move {
                self.record("POST", path, &body);
                self.decode()
            }
        }

        fn patch_json<'a, T>(
            &'a self,
            path: &'a str,
            body: Vec<u8>,
        ) -> impl Future<Output = Result<T, String>> + Send + 'a
        where
            T: DeserializeOwned + Send + 'static,
        {
            async move {
                self.record("PATCH", path, &body);
                self.decode()
            }
        }

        fn post_empty<'a>(
            &'a self,
            path: &'a str,
            body: Vec<u8>,
        ) -> impl Future<Output = Result<(), String>> + Send + 'a {
            async move {
                self.record("POST", path, &body);
                Ok(())
            }
        }

        fn put_empty<'a>(
            &'a self,
            path: &'a str,
            body: Vec<u8>,
        ) -> impl Future<Output = Result<(), String>> + Send + 'a {
            async move {
                self.record("PUT", path, &body);
                Ok(())
            }
        }

        fn delete_empty<'a>(
            &'a self,
            path: &'a str,
        ) -> impl Future<Output = Result<(), String>> + Send + 'a {
            async move {
                self.record("DELETE", path, b"");
                Ok(())
            }
        }

        fn post_raw<'a>(
            &'a self,
            path: &'a str,
            body: Vec<u8>,
            _retryable: bool,
        ) -> impl Future<Output = Result<Vec<u8>, String>> + Send + 'a {
            async move {
                self.record("POST", path, &body);
                Ok(self.canned.clone().into_bytes())
            }
        }
    }

    const SYNC_RESPONSE: &str = r#"{"rows":[{"id":"r1"}],"next_cursor":7,"rebased":false,"oldest_ordinal":1,"has_more_older":true}"#;

    /// THE pin: a baseline sync (null cursor) must serialize `since_ordinal` as an
    /// EXPLICIT null. Add `skip_serializing_if = "Option::is_none"` and the field
    /// vanishes, the web side reads `undefined` instead of `null`, and a baseline
    /// REPLACE silently becomes an APPEND — a blank or duplicated transcript. That
    /// bug class has shipped twice.
    #[tokio::test]
    async fn a_baseline_sync_page_carries_since_ordinal_as_an_explicit_null() {
        let client = RecordingClient::new(SYNC_RESPONSE);
        let frame = fetch_sync(&client, "s1".to_string(), None, 50)
            .await
            .expect("sync");

        assert_eq!(
            frame,
            r#"{"kind":"sync_page","rows":[{"id":"r1"}],"since_ordinal":null,"next_cursor":7,"rebased":false,"oldest_ordinal":1,"has_more_older":true}"#
        );
        assert!(
            frame.contains(r#""since_ordinal":null"#),
            "the baseline marker must survive as a literal null: {frame}"
        );
        // A never-compacted session omits the field entirely (not `[]`), so the
        // common frame shape is unchanged.
        assert!(
            !frame.contains("compaction_points"),
            "no boundaries ⇒ no field: {frame}"
        );
    }

    /// A compacted session's sync carries its boundaries through to the webview
    /// verbatim, so the pre-compaction divider survives a warm re-entry's
    /// difference sync (not just a baseline).
    #[tokio::test]
    async fn a_sync_page_carries_compaction_points_verbatim() {
        let client = RecordingClient::new(
            r#"{"rows":[{"id":"m7"}],"next_cursor":7,"rebased":false,"oldest_ordinal":0,"has_more_older":false,"compaction_points":[{"ordinal":3,"at":"2026-07-22T10:00:00Z"}]}"#,
        );
        let frame = fetch_sync(&client, "s1".to_string(), Some(2), 50)
            .await
            .expect("sync");
        let json: serde_json::Value = serde_json::from_str(&frame).expect("parse");
        assert_eq!(json["compaction_points"][0]["ordinal"], 3);
        assert_eq!(json["compaction_points"][0]["at"], "2026-07-22T10:00:00Z");
    }

    #[tokio::test]
    async fn a_resumed_sync_page_echoes_the_cursor_it_asked_for() {
        let client = RecordingClient::new(SYNC_RESPONSE);
        let frame = fetch_sync(&client, "s1".to_string(), Some(12), 50)
            .await
            .expect("sync");

        let json: serde_json::Value = serde_json::from_str(&frame).expect("parse");
        assert_eq!(json["kind"], KIND_SYNC_PAGE);
        assert_eq!(json["since_ordinal"], 12);
        assert_eq!(json["next_cursor"], 7);
        assert_eq!(json["rebased"], false);
    }

    /// A null `next_cursor` / `oldest_ordinal` must stay a literal null too — the
    /// web handler reads them directly off the frame.
    #[tokio::test]
    async fn a_sync_page_keeps_its_null_cursor_fields() {
        let client = RecordingClient::new(
            r#"{"rows":[],"next_cursor":null,"rebased":true,"oldest_ordinal":null,"has_more_older":false}"#,
        );
        let frame = fetch_sync(&client, "s1".to_string(), None, 50)
            .await
            .expect("sync");

        assert_eq!(
            frame,
            r#"{"kind":"sync_page","rows":[],"since_ordinal":null,"next_cursor":null,"rebased":true,"oldest_ordinal":null,"has_more_older":false}"#
        );
    }

    #[tokio::test]
    async fn the_sync_query_opens_with_a_question_mark_then_ampersands() {
        let client = RecordingClient::new(SYNC_RESPONSE);
        fetch_sync(&client, "s1".to_string(), Some(12), 50)
            .await
            .expect("sync");
        assert_eq!(
            client.only_call().path,
            "/v1/chat/sessions/s1/sync?since_ordinal=12&limit=50"
        );
    }

    /// The baseline pull omits the cursor from the QUERY (the server reads absence
    /// as "newest page") while still declaring it as null in the FRAME.
    #[tokio::test]
    async fn a_baseline_sync_query_carries_only_the_limit() {
        let client = RecordingClient::new(SYNC_RESPONSE);
        fetch_sync(&client, "s1".to_string(), None, 30)
            .await
            .expect("sync");
        assert_eq!(
            client.only_call().path,
            "/v1/chat/sessions/s1/sync?limit=30"
        );
    }

    #[tokio::test]
    async fn a_history_page_frame_is_tagged_history_page() {
        let client = RecordingClient::new(
            r#"{"transcript":[{"id":"h1"}],"has_more":true,"oldest_ordinal":4,"newest_ordinal":9}"#,
        );
        let frame = fetch_history_page(&client, "s1".to_string(), Some(10), Some(20))
            .await
            .expect("history");

        assert_eq!(
            frame,
            r#"{"kind":"history_page","rows":[{"id":"h1"}],"oldest_ordinal":4,"newest_ordinal":9,"has_more":true}"#
        );
        let json: serde_json::Value = serde_json::from_str(&frame).expect("parse");
        assert_eq!(json["kind"], KIND_HISTORY_PAGE);
        assert_eq!(
            client.only_call().path,
            "/v1/chat/sessions/s1?before_ordinal=10&limit=20"
        );
    }

    /// No `before_ordinal`, no `limit` → no query string at all (not a bare `?`).
    #[tokio::test]
    async fn a_history_page_without_params_has_no_query_string() {
        let client = RecordingClient::new(
            r#"{"transcript":[],"has_more":false,"oldest_ordinal":null,"newest_ordinal":null}"#,
        );
        fetch_history_page(&client, "s1".to_string(), None, None)
            .await
            .expect("history");
        assert_eq!(client.only_call().path, "/v1/chat/sessions/s1");
    }

    /// Only `limit` given: it must still open the query with `?`, never `&`.
    #[tokio::test]
    async fn a_lone_trailing_query_param_still_opens_with_a_question_mark() {
        let client = RecordingClient::new(
            r#"{"transcript":[],"has_more":false,"oldest_ordinal":null,"newest_ordinal":null}"#,
        );
        fetch_history_page(&client, "s1".to_string(), None, Some(20))
            .await
            .expect("history");
        assert_eq!(client.only_call().path, "/v1/chat/sessions/s1?limit=20");
    }

    /// A session id carrying a path/query character would silently retarget the
    /// request at another endpoint.
    #[tokio::test]
    async fn a_session_id_that_could_escape_its_path_segment_is_rejected() {
        for bad in ["", "a/b", "a?b", "a#b", "../admin"] {
            let client = RecordingClient::empty();
            let err = set_pinned(&client, bad.to_string(), true)
                .await
                .expect_err("must reject {bad}");
            assert_eq!(err, "invalid session_id");
            assert!(client.calls.lock().is_empty(), "{bad} must not be sent");
        }
    }

    /// The gateway's `ChatSubagentList`, verbatim — including the keys it SKIPS
    /// on a child that has nothing to put in them (`subagent_type`, `task`,
    /// `started_at`, `ended_at`).
    const SUBAGENT_LIST: &str = r#"{
        "items":[
            {"session_id":"c1","subagent_type":"explorer","backend":"baybo","task":"Find every caller","status":"completed","created_at":"2026-08-13T01:00:00Z","started_at":"2026-08-13T01:00:01Z","ended_at":"2026-08-13T01:02:00Z"},
            {"session_id":"c2","backend":"codex","status":"pending","created_at":"2026-08-13T01:03:00Z"}
        ],
        "has_more_older":true
    }"#;

    #[tokio::test]
    async fn list_subagents_reads_the_children_off_the_parents_path() {
        let client = RecordingClient::new(SUBAGENT_LIST);
        let list = list_subagents(&client, "s1".to_string(), None)
            .await
            .expect("subagents");

        let call = client.only_call();
        assert_eq!(call.method, "GET");
        assert_eq!(call.path, "/v1/chat/sessions/s1/subagents");
        assert!(list.has_more_older);

        let first = &list.items[0];
        assert_eq!(first.session_id, "c1");
        assert_eq!(first.subagent_type.as_deref(), Some("explorer"));
        assert_eq!(first.backend, "baybo");
        assert_eq!(first.task.as_deref(), Some("Find every caller"));
        assert_eq!(first.status, ChatSubagentStatus::Completed);
        assert_eq!(first.created_at, "2026-08-13T01:00:00Z");
        assert_eq!(first.started_at.as_deref(), Some("2026-08-13T01:00:01Z"));
        assert_eq!(first.ended_at.as_deref(), Some("2026-08-13T01:02:00Z"));

        // A child that has not opened a turn yet still owes the sheet a row.
        let second = &list.items[1];
        assert_eq!(second.backend, "codex");
        assert_eq!(second.subagent_type, None);
        assert_eq!(second.task, None);
        assert_eq!(second.status, ChatSubagentStatus::Pending);
        assert_eq!(second.started_at, None);
        assert_eq!(second.ended_at, None);
    }

    /// A status this build has never heard of costs that ONE row its label. A
    /// strict decode would cost the whole sheet.
    #[tokio::test]
    async fn an_unrecognized_subagent_status_reads_as_unknown() {
        let client = RecordingClient::new(
            r#"{"items":[{"session_id":"c1","backend":"baybo","status":"evaporated","created_at":"2026-08-13T01:00:00Z"}],"older_omitted":0}"#,
        );
        let list = list_subagents(&client, "s1".to_string(), None)
            .await
            .expect("subagents");
        assert_eq!(list.items[0].status, ChatSubagentStatus::Unknown);
    }

    #[tokio::test]
    async fn list_subagents_reads_an_empty_body_as_an_empty_listing() {
        let client = RecordingClient::new("{}");
        let list = list_subagents(&client, "s1".to_string(), None)
            .await
            .expect("subagents");
        assert!(list.items.is_empty());
        assert!(!list.has_more_older);
    }

    /// A child's transcript rides its OWN path space — `/v1/chat/sessions/{id}`
    /// 404s a child (it is not on the `owner` channel) — and comes back as the
    /// same frame the transcript bundle already consumes.
    #[tokio::test]
    async fn a_subagent_history_page_rides_the_subagent_path() {
        let client = RecordingClient::new(
            r#"{"transcript":[{"id":"h1"}],"has_more":true,"oldest_ordinal":4,"newest_ordinal":9}"#,
        );
        let frame = fetch_subagent_history_page(&client, "c1".to_string(), Some(10), Some(20))
            .await
            .expect("history");

        let json: serde_json::Value = serde_json::from_str(&frame).expect("parse");
        assert_eq!(json["kind"], KIND_HISTORY_PAGE);
        assert_eq!(json["rows"][0]["id"], "h1");
        assert_eq!(
            client.only_call().path,
            "/v1/chat/subagents/c1?before_ordinal=10&limit=20"
        );
    }

    #[tokio::test]
    async fn a_subagent_history_page_without_params_has_no_query_string() {
        let client = RecordingClient::new(
            r#"{"transcript":[],"has_more":false,"oldest_ordinal":null,"newest_ordinal":null}"#,
        );
        fetch_subagent_history_page(&client, "c1".to_string(), None, None)
            .await
            .expect("history");
        assert_eq!(client.only_call().path, "/v1/chat/subagents/c1");
    }

    #[tokio::test]
    async fn a_subagent_sync_query_opens_with_a_question_mark_then_ampersands() {
        let client = RecordingClient::new(SYNC_RESPONSE);
        let frame = fetch_subagent_sync(&client, "c1".to_string(), Some(12), 50)
            .await
            .expect("sync");

        let json: serde_json::Value = serde_json::from_str(&frame).expect("parse");
        assert_eq!(json["kind"], KIND_SYNC_PAGE);
        assert_eq!(json["since_ordinal"], 12);
        assert_eq!(
            client.only_call().path,
            "/v1/chat/subagents/c1/sync?since_ordinal=12&limit=50"
        );
    }

    /// The child page opens on a baseline pull, so the REPLACE marker matters
    /// here exactly as much as on the parent: cursor absent from the QUERY,
    /// explicit null in the FRAME.
    #[tokio::test]
    async fn a_baseline_subagent_sync_carries_only_the_limit_and_a_null_cursor() {
        let client = RecordingClient::new(SYNC_RESPONSE);
        let frame = fetch_subagent_sync(&client, "c1".to_string(), None, 30)
            .await
            .expect("sync");

        assert!(
            frame.contains(r#""since_ordinal":null"#),
            "the baseline marker must survive as a literal null: {frame}"
        );
        assert_eq!(
            client.only_call().path,
            "/v1/chat/subagents/c1/sync?limit=30"
        );
    }

    /// A child id is whatever the gateway's listing said, and it reaches all
    /// three routes as a path segment.
    #[tokio::test]
    async fn the_subagent_routes_percent_encode_the_child_id() {
        let child = "c 1%2";

        let client = RecordingClient::new(SUBAGENT_LIST);
        list_subagents(&client, child.to_string(), None)
            .await
            .expect("subagents");
        assert_eq!(
            client.only_call().path,
            "/v1/chat/sessions/c%201%252/subagents"
        );

        let client = RecordingClient::new(r#"{"transcript":[],"has_more":false}"#);
        fetch_subagent_history_page(&client, child.to_string(), None, None)
            .await
            .expect("history");
        assert_eq!(client.only_call().path, "/v1/chat/subagents/c%201%252");

        let client = RecordingClient::new(SYNC_RESPONSE);
        fetch_subagent_sync(&client, child.to_string(), None, 30)
            .await
            .expect("sync");
        assert_eq!(
            client.only_call().path,
            "/v1/chat/subagents/c%201%252/sync?limit=30"
        );
    }

    /// An empty id would target the collection route rather than a child, and a
    /// path character would retarget the request outright.
    #[tokio::test]
    async fn the_subagent_routes_reject_an_id_that_could_escape_its_path_segment() {
        for bad in ["", "a/b", "a?b", "a#b", "../sessions/s1"] {
            let client = RecordingClient::empty();
            assert!(
                list_subagents(&client, bad.to_string(), None)
                    .await
                    .is_err(),
                "{bad:?} must be rejected"
            );
            assert!(
                fetch_subagent_history_page(&client, bad.to_string(), None, None)
                    .await
                    .is_err(),
                "{bad:?} must be rejected"
            );
            assert!(
                fetch_subagent_sync(&client, bad.to_string(), None, 30)
                    .await
                    .is_err(),
                "{bad:?} must be rejected"
            );
            assert!(client.calls.lock().is_empty(), "{bad:?} must not be sent");
        }
    }

    /// A CJK query is the normal case here, and it must survive the query
    /// string intact — the index makes every Han codepoint its own token, so a
    /// mangled byte is not a degraded search, it is a different one.
    #[tokio::test]
    async fn a_search_percent_encodes_a_cjk_query() {
        let client = RecordingClient::new(r#"{"groups":[],"truncated":false}"#);
        search_messages(&client, "数据库 迁移")
            .await
            .expect("search");

        assert_eq!(
            client.only_call().path,
            "/v1/chat/search?q=%E6%95%B0%E6%8D%AE%E5%BA%93%20%E8%BF%81%E7%A7%BB"
        );
    }

    /// Only `q` goes on the wire. Every scope flag is the gateway's default, and
    /// sending one from here would silently diverge from app/web's panel, which
    /// sends `q` alone.
    #[tokio::test]
    async fn a_search_sends_only_the_query() {
        let client = RecordingClient::new(r#"{"groups":[],"truncated":false}"#);
        search_messages(&client, "hello").await.expect("search");

        let call = client.only_call();
        assert_eq!(call.path, "/v1/chat/search?q=hello");
        assert_eq!(call.method, "GET");
    }

    /// A hostile query is quoted into a literal phrase server-side, so nothing
    /// here needs to sanitize it — but it must still reach the gateway byte-for
    /// -byte rather than being cut short by an unescaped `&` or `#`.
    #[tokio::test]
    async fn a_search_query_cannot_smuggle_extra_parameters() {
        let client = RecordingClient::new(r#"{"groups":[],"truncated":false}"#);
        search_messages(&client, "a&include_hidden=true#x")
            .await
            .expect("search");

        assert_eq!(
            client.only_call().path,
            "/v1/chat/search?q=a%26include_hidden%3Dtrue%23x"
        );
    }

    #[tokio::test]
    async fn a_search_carries_every_group_field_through() {
        let client = RecordingClient::new(
            r#"{"groups":[{"session_id":"s1","session_title":"迁移计划",
                 "total_hits":7,
                 "hits":[{"ordinal":12,"role":"user","text":"数据库迁移怎么做",
                          "created_at":"2026-08-12T03:04:05Z","superseded_by":40},
                         {"ordinal":13,"role":"assistant","text":"先备份",
                          "created_at":"2026-08-12T03:04:09Z"}]}],
               "truncated":true}"#,
        );
        let results = search_messages(&client, "迁移").await.expect("search");

        assert!(results.truncated);
        let group = &results.groups[0];
        assert_eq!(group.session_id, "s1");
        assert_eq!(group.session_title.as_deref(), Some("迁移计划"));
        assert_eq!(group.total_hits, 7);
        assert_eq!(group.hits.len(), 2);
        assert_eq!(group.hits[0].ordinal, 12);
        assert_eq!(group.hits[0].role, "user");
        assert_eq!(group.hits[0].text, "数据库迁移怎么做");
        assert_eq!(group.hits[0].created_at, "2026-08-12T03:04:05Z");
        assert_eq!(group.hits[0].superseded_by, Some(40));
        assert_eq!(group.hits[1].superseded_by, None);
    }

    /// An older gateway that omits the optional keys must degrade to a usable
    /// result, never to a decode error that blanks the whole search.
    #[tokio::test]
    async fn a_search_tolerates_a_gateway_without_the_optional_fields() {
        let client = RecordingClient::new(
            r#"{"groups":[{"session_id":"s1",
                 "hits":[{"ordinal":1,"role":"user","created_at":"2026-08-12T03:04:05Z"}]}]}"#,
        );
        let results = search_messages(&client, "x").await.expect("search");

        assert!(!results.truncated);
        let group = &results.groups[0];
        assert_eq!(group.session_title, None);
        assert_eq!(group.total_hits, 0);
        assert_eq!(group.hits[0].text, "");
        assert_eq!(group.hits[0].superseded_by, None);
    }

    #[tokio::test]
    async fn a_message_lookup_percent_encodes_its_key() {
        let client = RecordingClient::new(r#"{"found":true,"ordinal":3}"#);
        let found = lookup_message(&client, "s1".to_string(), "a b&c=d/e~f.g_h-i%")
            .await
            .expect("lookup");

        assert!(found.found);
        assert_eq!(found.ordinal, Some(3));
        assert_eq!(
            client.only_call().path,
            "/v1/chat/sessions/s1/messages?platform_msg_id=a%20b%26c%3Dd%2Fe~f.g_h-i%25"
        );
    }

    #[tokio::test]
    async fn a_message_lookup_percent_encodes_non_ascii_keys() {
        let client = RecordingClient::new(r#"{"found":false}"#);
        let found = lookup_message(&client, "s1".to_string(), "é")
            .await
            .expect("lookup");

        assert!(!found.found);
        assert_eq!(found.ordinal, None);
        assert_eq!(
            client.only_call().path,
            "/v1/chat/sessions/s1/messages?platform_msg_id=%C3%A9"
        );
    }

    #[tokio::test]
    async fn a_blank_platform_msg_id_never_reaches_the_gateway() {
        let client = RecordingClient::empty();
        let rejected = lookup_message(&client, "s1".to_string(), "   ").await;
        assert_eq!(rejected.err().as_deref(), Some("invalid platform_msg_id"));
        assert!(client.calls.lock().is_empty());
    }

    #[tokio::test]
    async fn create_session_posts_the_requested_id() {
        let client = RecordingClient::new(r#"{"session_id":"s1"}"#);
        let created = create_session(&client, "s1").await.expect("create");

        assert_eq!(created, "s1");
        assert_eq!(
            client.only_call(),
            RecordedCall {
                method: "POST",
                path: "/v1/chat/sessions".to_string(),
                body: r#"{"session_id":"s1"}"#.to_string(),
            }
        );
    }

    /// The list row's optional fields are all `#[serde(default)]` — an older
    /// gateway that predates them must still populate a row rather than fail the
    /// whole list.
    #[tokio::test]
    async fn list_sessions_tolerates_a_gateway_without_the_optional_fields() {
        let client = RecordingClient::new(
            r#"{"items":[{"session_id":"s1","created_at":"2026-07-12T00:00:00Z","last_active":"2026-07-12T00:01:00Z","pinned":true}]}"#,
        );
        let rows = list_sessions(&client).await.expect("list");

        assert_eq!(client.only_call().path, PATH_CHAT_SESSIONS);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.session_id, "s1");
        assert!(row.pinned);
        assert!(!row.archived);
        assert_eq!(row.unread_count, 0);
        assert_eq!(row.last_message_text, None);
        assert_eq!(row.last_user_text, None);
        assert_eq!(row.title, None);
        assert_eq!(row.cron_job_id, None);
        assert_eq!(row.cron_job_title, None);
        // The gateway skips the key when nothing is parked (and an older one
        // never sends it at all) — both must read as "nothing is waiting".
        assert!(!row.approval_pending);
    }

    #[tokio::test]
    async fn list_sessions_carries_every_row_field_through() {
        let client = RecordingClient::new(
            r#"{"items":[{"session_id":"s1","created_at":"c","last_active":"l","last_user_text":"hi","last_message_text":"reply","title":"A chat","pinned":false,"archived":true,"unread_count":3,"approval_pending":true,"cron_job_id":"cj-1","cron_job_title":"Morning brief"}]}"#,
        );
        let rows = list_sessions(&client).await.expect("list");

        let row = &rows[0];
        assert_eq!(row.last_user_text.as_deref(), Some("hi"));
        assert_eq!(row.last_message_text.as_deref(), Some("reply"));
        assert_eq!(row.title.as_deref(), Some("A chat"));
        assert!(row.archived);
        assert_eq!(row.unread_count, 3);
        assert_eq!(row.cron_job_id.as_deref(), Some("cj-1"));
        assert_eq!(row.cron_job_title.as_deref(), Some("Morning brief"));
        assert!(row.approval_pending);
    }

    /// The whole reason the batch route exists: ONE round-trip, and no ordinal —
    /// the chat list holds none, so the gateway resolves each session's tail.
    #[tokio::test]
    async fn mark_many_read_posts_every_id_in_one_call() {
        let client = RecordingClient::empty();
        mark_many_read(&client, vec!["s1".to_string(), "s2".to_string()])
            .await
            .expect("batch read");
        assert_eq!(
            client.only_call(),
            RecordedCall {
                method: "POST",
                path: "/v1/chat/sessions/read".to_string(),
                body: r#"{"session_ids":["s1","s2"]}"#.to_string(),
            }
        );
    }

    /// An empty group (every fire pinned or archived away) must not fire a
    /// pointless request.
    #[tokio::test]
    async fn mark_many_read_with_no_ids_is_a_no_op() {
        let client = RecordingClient::empty();
        mark_many_read(&client, Vec::new()).await.expect("no-op");
        assert!(client.calls.lock().is_empty());
    }

    #[tokio::test]
    async fn set_archived_puts_the_flag_on_the_archive_path() {
        let client = RecordingClient::empty();
        set_archived(&client, "s1".to_string(), true)
            .await
            .expect("archive");
        assert_eq!(
            client.only_call(),
            RecordedCall {
                method: "PUT",
                path: "/v1/chat/sessions/s1/archive".to_string(),
                body: r#"{"archived":true}"#.to_string(),
            }
        );
    }

    #[tokio::test]
    async fn set_pinned_puts_the_flag_on_the_pin_path() {
        let client = RecordingClient::empty();
        set_pinned(&client, "s1".to_string(), false)
            .await
            .expect("pin");
        assert_eq!(
            client.only_call(),
            RecordedCall {
                method: "PUT",
                path: "/v1/chat/sessions/s1/pin".to_string(),
                body: r#"{"pinned":false}"#.to_string(),
            }
        );
    }

    /// The rename rides the session's own `/title` path with the title as a JSON
    /// STRING — the one call in this file whose body is user text, so an escape
    /// bug here would ship a malformed request for any title holding a quote.
    #[tokio::test]
    async fn set_title_puts_the_name_on_the_title_path() {
        let client = RecordingClient::empty();
        set_title(&client, "s1".to_string(), r#"Ship "it""#.to_string())
            .await
            .expect("rename");
        assert_eq!(
            client.only_call(),
            RecordedCall {
                method: "PUT",
                path: "/v1/chat/sessions/s1/title".to_string(),
                body: r#"{"title":"Ship \"it\""}"#.to_string(),
            }
        );
    }

    /// A cron group's pin is keyed by the JOB and rides `/v1/cron` — a different
    /// path space from every other call this client makes. Pinning the group must
    /// never be routed at a session id.
    #[tokio::test]
    async fn set_cron_pinned_puts_the_flag_on_the_jobs_pin_path() {
        let client = RecordingClient::empty();
        set_cron_pinned(&client, "job-1".to_string(), true)
            .await
            .expect("pin group");
        assert_eq!(
            client.only_call(),
            RecordedCall {
                method: "PUT",
                path: "/v1/cron/job-1/pin".to_string(),
                body: r#"{"pinned":true}"#.to_string(),
            }
        );
    }

    #[tokio::test]
    async fn set_cron_pinned_rejects_a_path_traversing_job_id() {
        let client = RecordingClient::empty();
        let err = set_cron_pinned(&client, "../sessions/s1".to_string(), true)
            .await
            .expect_err("a job id is a single path segment");
        assert!(err.contains("job_id"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn mark_read_puts_the_ordinal_on_the_read_path() {
        let client = RecordingClient::empty();
        mark_read(&client, "s1".to_string(), 7).await.expect("read");
        assert_eq!(
            client.only_call(),
            RecordedCall {
                method: "PUT",
                path: "/v1/chat/sessions/s1/read".to_string(),
                body: r#"{"ordinal":7}"#.to_string(),
            }
        );
    }

    /// The request is the feature's whole scope contract: live (no `deleted`)
    /// and `owner` only. A regression here is invisible in the UI until someone
    /// else's Telegram job shows up in the list.
    #[tokio::test]
    async fn list_cron_jobs_asks_for_the_owner_channel_and_never_the_bin() {
        let client = RecordingClient::new(r#"{"items":[]}"#);
        let jobs = list_cron_jobs(&client).await.expect("list");

        assert!(jobs.is_empty());
        let call = client.only_call();
        assert_eq!(call.method, "GET");
        assert_eq!(call.path, "/v1/cron?channel=owner");
        assert!(
            !call.path.contains("deleted"),
            "the phone must never ask for the recycle bin: {}",
            call.path,
        );
    }

    /// Both schedule kinds ride the same list — the user asked for every live
    /// Every recurring job survives with its fields intact, and a one-shot is
    /// dropped whole — never mangled into an expression, never counted.
    #[tokio::test]
    async fn list_cron_jobs_carries_recurring_jobs_and_drops_one_shots() {
        let client = RecordingClient::new(
            r#"{"items":[
                {"id":"j1","title":"Morning brief","prompt":"Summarize","schedule":{"kind":"cron","expr":"0 9 * * *"},"timezone":"Asia/Shanghai","status":"enabled","next_trigger_at":"2026-07-18T01:00:00Z","last_triggered_at":"2026-07-17T01:00:00Z"},
                {"id":"j2","title":"A reminder","prompt":"One shot","schedule":{"kind":"at","time":"2026-07-20T10:00:00Z"},"timezone":"UTC","status":"executed"},
                {"id":"j3","title":"","prompt":"p","schedule":{"kind":"cron","expr":"0 18 * * FRI"},"timezone":"UTC","status":"disabled"}
            ]}"#,
        );
        let jobs = list_cron_jobs(&client).await.expect("list");

        assert_eq!(
            jobs.iter().map(|j| j.id.as_str()).collect::<Vec<_>>(),
            ["j1", "j3"],
            "a one-shot is a reminder, not a schedule this list manages",
        );

        assert_eq!(jobs[0].title, "Morning brief");
        assert_eq!(jobs[0].expr, "0 9 * * *");
        assert_eq!(jobs[0].timezone, "Asia/Shanghai");
        assert_eq!(jobs[0].status, CronJobStatus::Enabled);
        assert_eq!(
            jobs[0].next_trigger_at.as_deref(),
            Some("2026-07-18T01:00:00Z")
        );
        assert_eq!(
            jobs[0].last_triggered_at.as_deref(),
            Some("2026-07-17T01:00:00Z")
        );

        // A legacy row: no title, and the list falls back to the prompt — so the
        // empty string must survive rather than becoming a `None` nobody checks.
        assert_eq!(jobs[1].title, "");
        assert_eq!(jobs[1].prompt, "p");
        // Paused: it keeps its expression and has nothing coming.
        assert_eq!(jobs[1].expr, "0 18 * * FRI");
        assert_eq!(jobs[1].status, CronJobStatus::Disabled);
        assert_eq!(jobs[1].next_trigger_at, None);
    }

    /// A list of nothing but one-shots is EMPTY — not a decode error, and not a
    /// list of holes. `[]` is what the screen renders "no scheduled jobs" from.
    #[tokio::test]
    async fn a_list_of_only_one_shots_comes_back_empty() {
        let client = RecordingClient::new(
            r#"{"items":[
                {"id":"j1","title":"Renew the cert","prompt":"x","schedule":{"kind":"at","time":"2026-07-20T10:00:00Z"},"timezone":"UTC","status":"executed"},
                {"id":"j2","title":"Call mum","prompt":"y","schedule":{"kind":"at","time":"2026-08-01T10:00:00Z"},"timezone":"UTC","status":"enabled"}
            ]}"#,
        );
        let jobs = list_cron_jobs(&client).await.expect("list");
        assert!(jobs.is_empty(), "got {jobs:?}");
    }

    #[tokio::test]
    async fn hide_session_deletes_the_session_path() {
        let client = RecordingClient::empty();
        hide_session(&client, "s1".to_string()).await.expect("hide");
        assert_eq!(
            client.only_call(),
            RecordedCall {
                method: "DELETE",
                path: "/v1/chat/sessions/s1".to_string(),
                body: String::new(),
            }
        );
    }

    /// One round-trip, one body — a group's fires are hidden together or not at
    /// all, so looping [`hide_session`] over them is the thing this replaces.
    #[tokio::test]
    async fn hide_many_posts_every_id_in_one_call() {
        let client = RecordingClient::empty();
        hide_many(&client, vec!["s1".to_string(), "s2".to_string()])
            .await
            .expect("batch hide");
        assert_eq!(
            client.only_call(),
            RecordedCall {
                method: "POST",
                path: "/v1/chat/sessions/hide".to_string(),
                body: r#"{"session_ids":["s1","s2"]}"#.to_string(),
            }
        );
    }

    #[tokio::test]
    async fn pausing_and_resuming_a_cron_job_post_their_own_verbs() {
        let client = RecordingClient::empty();
        set_cron_paused(&client, "j1".to_string(), true)
            .await
            .expect("pause");
        assert_eq!(
            client.only_call(),
            RecordedCall {
                method: "POST",
                path: "/v1/cron/j1/pause".to_string(),
                body: String::new(),
            }
        );

        let client = RecordingClient::empty();
        set_cron_paused(&client, "j1".to_string(), false)
            .await
            .expect("resume");
        assert_eq!(client.only_call().path, "/v1/cron/j1/resume");
    }

    #[tokio::test]
    async fn deleting_a_cron_job_deletes_the_job_path_not_a_session() {
        let client = RecordingClient::empty();
        delete_cron_job(&client, "j1".to_string())
            .await
            .expect("delete");
        assert_eq!(
            client.only_call(),
            RecordedCall {
                method: "DELETE",
                path: "/v1/cron/j1".to_string(),
                body: String::new(),
            }
        );
    }

    /// A job id reaches the gateway as a PATH segment, so it gets the same
    /// traversal guard `set_cron_pinned` has — nothing about these verbs makes
    /// the id trustworthier.
    #[tokio::test]
    async fn cron_mutations_reject_a_path_traversing_job_id() {
        let client = RecordingClient::empty();
        assert!(
            set_cron_paused(&client, "../../v1/chat/sessions".to_string(), true)
                .await
                .is_err()
        );
        assert!(
            delete_cron_job(&client, "../../v1/chat/sessions".to_string())
                .await
                .is_err()
        );
        assert!(client.calls.lock().is_empty());
    }

    /// The confirm dialog snapshots the visible members, so an empty snapshot is
    /// reachable (every fire pinned or archived away between prompt and tap) and
    /// must not fire a pointless request.
    #[tokio::test]
    async fn hide_many_with_no_ids_is_a_no_op() {
        let client = RecordingClient::empty();
        hide_many(&client, Vec::new()).await.expect("no-op");
        assert!(client.calls.lock().is_empty());
    }

    /// The picker reads three fields off a row that carries a dashboard's worth
    /// of config/pricing detail — the extras must be dropped, not a decode error.
    #[tokio::test]
    async fn list_llm_models_narrows_the_dashboard_rows() {
        let client = RecordingClient::new(
            r#"{"default_name":"fast","items":[
                {"name":"fast","provider":"anthropic","model":"claude-haiku-4-5","api_key_configured":true,"is_default":true,"effective_context_window":200000,"effective_supports_vision":true,"effective_pricing":{}},
                {"name":"5.5-max","provider":"openai","model":"gpt-5.5","model_list":[{"model":"gpt-5.5"},{"model":"o3","context_window":200000}],"reasoning_effort":"xhigh","available_efforts":["low","medium","high","xhigh","max"],"api_key_configured":false,"is_default":false,"effective_context_window":400000,"effective_supports_vision":false,"effective_pricing":{}}
            ]}"#,
        );
        let catalog = list_llm_models(&client).await.expect("models");

        assert_eq!(client.only_call().path, "/v1/llm/models");
        assert_eq!(catalog.default_name, "fast");
        assert_eq!(catalog.items.len(), 2);
        assert_eq!(catalog.items[0].name, "fast");
        assert_eq!(catalog.items[0].provider, "anthropic");
        assert_eq!(catalog.items[0].model, "claude-haiku-4-5");
        // No override on the row → provider default, not a decode error.
        assert_eq!(catalog.items[0].reasoning_effort, None);
        // Absent model list decodes to empty, not an error.
        assert!(catalog.items[0].model_candidates.is_empty());
        assert_eq!(catalog.items[1].name, "5.5-max");
        assert_eq!(catalog.items[1].reasoning_effort.as_deref(), Some("xhigh"));
        // Each row's per-model overrides are dropped; only the ids reach the
        // picker, and the entry's default is already among them.
        assert_eq!(catalog.items[1].model_candidates, ["gpt-5.5", "o3"]);
        // The rungs this provider can be told drive the Thinking sub-level.
        assert_eq!(
            catalog.items[1].available_efforts,
            ["low", "medium", "high", "xhigh", "max"]
        );
        // A gateway that sent no list (or a provider baybo tells nothing)
        // decodes to empty, and the panel hides the row rather than offering
        // picks that would never reach the wire.
        assert!(catalog.items[0].available_efforts.is_empty());
    }

    /// The pin read rides the session detail with `limit=1` — the smallest page
    /// the route serves; the transcript row along for the ride is dropped. Both
    /// the entry and the model-within-it come back.
    #[tokio::test]
    async fn fetch_session_model_reads_the_pin_off_a_one_row_detail() {
        let client = RecordingClient::new(
            r#"{"session_id":"s1","transcript":[{"id":"r9"}],"has_more":true,"last_llm":"5.5-max","last_model":"o3","last_effort":"high"}"#,
        );
        let pin = fetch_session_model(&client, "s1".to_string())
            .await
            .expect("pin");

        assert_eq!(pin.llm.as_deref(), Some("5.5-max"));
        assert_eq!(pin.model.as_deref(), Some("o3"));
        assert_eq!(pin.effort.as_deref(), Some("high"));
        assert_eq!(client.only_call().path, "/v1/chat/sessions/s1?limit=1");
    }

    /// An unpinned session's detail SKIPS the pin fields server-side
    /// (`skip_serializing_if`) — absence must read as "follow the default".
    #[tokio::test]
    async fn fetch_session_model_reads_an_absent_pin_as_none() {
        let client =
            RecordingClient::new(r#"{"session_id":"s1","transcript":[],"has_more":false}"#);
        let pin = fetch_session_model(&client, "s1".to_string())
            .await
            .expect("pin");
        assert_eq!(pin.llm, None);
        assert_eq!(pin.model, None);
        assert_eq!(pin.effort, None);
    }

    #[tokio::test]
    async fn set_session_model_puts_the_pin_on_the_model_path() {
        let client = RecordingClient::empty();
        set_session_model(
            &client,
            "s1".to_string(),
            Some("5.5-max".to_string()),
            Some("o3".to_string()),
            Some("high".to_string()),
        )
        .await
        .expect("pin");
        assert_eq!(
            client.only_call(),
            RecordedCall {
                method: "PUT",
                path: "/v1/chat/sessions/s1/model".to_string(),
                body: r#"{"llm":"5.5-max","model":"o3","reasoning_effort":"high"}"#.to_string(),
            }
        );
    }

    /// THE pin: clearing must send EXPLICIT nulls — the route's "follow
    /// `default-llm`, default model + effort" request — never an empty object.
    #[tokio::test]
    async fn clearing_the_session_model_sends_explicit_nulls() {
        let client = RecordingClient::empty();
        set_session_model(&client, "s1".to_string(), None, None, None)
            .await
            .expect("clear");
        assert_eq!(
            client.only_call().body,
            r#"{"llm":null,"model":null,"reasoning_effort":null}"#
        );
    }

    #[tokio::test]
    async fn session_model_calls_reject_a_path_escaping_session_id() {
        for bad in ["a/b", "a?b", ""] {
            let client = RecordingClient::empty();
            assert!(
                fetch_session_model(&client, bad.to_string()).await.is_err(),
                "{bad:?} must be rejected"
            );
            assert!(
                set_session_model(&client, bad.to_string(), None, None, None)
                    .await
                    .is_err(),
                "{bad:?} must be rejected"
            );
            assert!(client.calls.lock().is_empty());
        }
    }

    /// The gateway's `DeckResponse` shape, verbatim — including the fields the
    /// server SKIPS on live/clean rows (`deleted_at_ms`, `error`), which must
    /// come back as `None` rather than failing the decode.
    const DECK_RESPONSE: &str = r#"{
        "cards":[
            {"card_id":"c1","title":"Claude quota","position":0,"size":"wide","enabled":true,"quarantined":false,"spec_hash":"h1","last_seq":41,"created_at_ms":1752000000000},
            {"card_id":"c2","title":"Machine status","position":1,"size":"small","enabled":false,"quarantined":true,"spec_hash":"h2","last_seq":7,"created_at_ms":1752100000000}
        ],
        "snapshots":[
            {"card_id":"c1","seq":41,"payload":"{\"used\":0.4}","fetched_at_ms":1752200000000},
            {"card_id":"c2","seq":7,"payload":"","fetched_at_ms":1752200100000,"error":"call timed out"}
        ]
    }"#;

    #[tokio::test]
    async fn fetch_deck_parses_the_gateway_deck_response() {
        let client = RecordingClient::new(DECK_RESPONSE);
        let view = fetch_deck(&client).await.expect("deck");

        assert_eq!(client.only_call().path, PATH_DECK);
        assert_eq!(view.cards.len(), 2);
        let card = &view.cards[0];
        assert_eq!(card.card_id, "c1");
        assert_eq!(card.title, "Claude quota");
        assert_eq!(card.position, 0);
        assert_eq!(card.size, "wide");
        assert!(card.enabled);
        assert!(!card.quarantined);
        assert_eq!(card.deleted_at_ms, None);
        assert_eq!(card.spec_hash, "h1");
        assert_eq!(card.last_seq, 41);
        assert_eq!(card.created_at_ms, 1_752_000_000_000);
        assert!(view.cards[1].quarantined);
        assert!(!view.cards[1].enabled);

        assert_eq!(view.snapshots.len(), 2);
        let clean = &view.snapshots[0];
        assert_eq!(clean.card_id, "c1");
        assert_eq!(clean.seq, 41);
        assert_eq!(clean.payload, r#"{"used":0.4}"#);
        assert_eq!(clean.fetched_at_ms, 1_752_200_000_000);
        assert_eq!(clean.error, None);
        let failed = &view.snapshots[1];
        assert_eq!(failed.payload, "");
        assert_eq!(failed.error.as_deref(), Some("call timed out"));
    }

    /// A recycle row is the same DTO with `deleted_at_ms` populated.
    #[tokio::test]
    async fn fetch_deck_recycle_parses_deleted_rows_off_the_recycle_path() {
        let client = RecordingClient::new(
            r#"[{"card_id":"c9","title":"Old card","position":3,"size":"large","enabled":false,"quarantined":false,"deleted_at_ms":1752300000000,"spec_hash":"h9","last_seq":2,"created_at_ms":1751000000000}]"#,
        );
        let cards = fetch_deck_recycle(&client).await.expect("recycle");

        assert_eq!(client.only_call().path, "/v1/deck/recycle");
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].card_id, "c9");
        assert_eq!(cards[0].deleted_at_ms, Some(1_752_300_000_000));
    }

    #[tokio::test]
    async fn fetch_deck_bundle_returns_the_card_html() {
        let client = RecordingClient::new(r#"{"card_html":"<main>quota</main>"}"#);
        let html = fetch_deck_bundle(&client, "c1".to_string())
            .await
            .expect("bundle");

        assert_eq!(html, "<main>quota</main>");
        assert_eq!(client.only_call().path, "/v1/deck/cards/c1/bundle");
    }

    /// The op call is a pass-through in BOTH directions: the params text goes
    /// up as the body byte-for-byte, and the response comes back byte-for-byte
    /// — never parsed and re-serialized, which would reorder the card's keys.
    #[tokio::test]
    async fn call_deck_op_passes_params_and_response_through_verbatim() {
        let canned = r#"{"zulu":1,"alpha":2}"#;
        let client = RecordingClient::new(canned);
        let result = call_deck_op(
            &client,
            "c1".to_string(),
            "quota".to_string(),
            r#"{"provider":"anthropic"}"#.to_string(),
            true,
        )
        .await
        .expect("op");

        assert_eq!(result, canned);
        assert_eq!(
            client.only_call(),
            RecordedCall {
                method: "POST",
                path: "/v1/deck/services/c1/quota".to_string(),
                body: r#"{"provider":"anthropic"}"#.to_string(),
            }
        );
    }

    /// The card id AND the op are both path segments the card's author never
    /// controls but the caller's JS might — same traversal guard as every
    /// other path-borne id.
    #[tokio::test]
    async fn call_deck_op_rejects_a_path_escaping_card_id_or_op() {
        for (card_id, op) in [("../cards/c1", "quota"), ("c1", "quota?x=1"), ("", "op")] {
            let client = RecordingClient::empty();
            assert!(
                call_deck_op(
                    &client,
                    card_id.to_string(),
                    op.to_string(),
                    "{}".to_string(),
                    false
                )
                .await
                .is_err(),
                "{card_id}/{op} must be rejected"
            );
            assert!(client.calls.lock().is_empty());
        }
    }

    #[tokio::test]
    async fn set_deck_layout_puts_the_full_ordered_layout() {
        let client = RecordingClient::empty();
        set_deck_layout(
            &client,
            vec![
                DeckLayoutEntryInput {
                    card_id: "c2".to_string(),
                    position: 0,
                    size: "large".to_string(),
                },
                DeckLayoutEntryInput {
                    card_id: "c1".to_string(),
                    position: 1,
                    size: "small".to_string(),
                },
            ],
        )
        .await
        .expect("layout");

        assert_eq!(
            client.only_call(),
            RecordedCall {
                method: "PUT",
                path: "/v1/deck/layout".to_string(),
                body: r#"[{"card_id":"c2","position":0,"size":"large"},{"card_id":"c1","position":1,"size":"small"}]"#.to_string(),
            }
        );
    }

    #[tokio::test]
    async fn set_deck_enabled_posts_its_own_verb_per_direction() {
        let client = RecordingClient::empty();
        set_deck_enabled(&client, "c1".to_string(), true)
            .await
            .expect("enable");
        assert_eq!(client.only_call().path, "/v1/deck/cards/c1/enable");

        let client = RecordingClient::empty();
        set_deck_enabled(&client, "c1".to_string(), false)
            .await
            .expect("disable");
        assert_eq!(client.only_call().path, "/v1/deck/cards/c1/disable");
    }

    #[tokio::test]
    async fn delete_deck_card_deletes_the_card_path() {
        let client = RecordingClient::empty();
        delete_deck_card(&client, "c1".to_string())
            .await
            .expect("delete");
        assert_eq!(
            client.only_call(),
            RecordedCall {
                method: "DELETE",
                path: "/v1/deck/cards/c1".to_string(),
                body: String::new(),
            }
        );
    }

    #[tokio::test]
    async fn restore_deck_card_posts_restore_and_parses_the_returned_row() {
        let client = RecordingClient::new(
            r#"{"card_id":"c9","title":"Old card","position":3,"size":"wide","enabled":true,"quarantined":false,"spec_hash":"h9","last_seq":2,"created_at_ms":1751000000000}"#,
        );
        let card = restore_deck_card(&client, "c9".to_string())
            .await
            .expect("restore");

        assert_eq!(client.only_call().path, "/v1/deck/cards/c9/restore");
        assert_eq!(card.card_id, "c9");
        assert!(card.enabled);
        assert_eq!(card.deleted_at_ms, None, "a restored row has left the bin");
    }

    /// Every deck mutation keyed by card id gets the same traversal guard.
    #[tokio::test]
    async fn deck_mutations_reject_a_path_traversing_card_id() {
        let client = RecordingClient::empty();
        assert!(
            set_deck_enabled(&client, "../x".to_string(), true)
                .await
                .is_err()
        );
        assert!(delete_deck_card(&client, "a/b".to_string()).await.is_err());
        assert!(restore_deck_card(&client, "a#b".to_string()).await.is_err());
        assert!(fetch_deck_bundle(&client, "a?b".to_string()).await.is_err());
        assert!(client.calls.lock().is_empty());
    }

    #[tokio::test]
    async fn update_apns_token_posts_the_token_and_its_environment() {
        let client = RecordingClient::empty();
        update_apns_token(&client, "abcd", "sandbox")
            .await
            .expect("apns");
        assert_eq!(
            client.only_call(),
            RecordedCall {
                method: "POST",
                path: PATH_MOBILE_APNS_TOKEN.to_string(),
                body: r#"{"apns_token":"abcd","apns_env":"sandbox"}"#.to_string(),
            }
        );
    }

    const ISSUE_RESPONSE: &str = r#"{"number":12,"project_id":"p1","title":"t","description":"d","status":"in_progress","priority":"high","position":3,"pinned":false,"stage":0,"unread":0,"last_run_failed":false,"approval_pending":true,"opened_by_agent":false,"created_at_ms":1,"updated_at_ms":2}"#;

    /// THE pin for the card patch. The gateway reads `assignee` and
    /// `blocked_reason` as double options: absent leaves the field alone, an
    /// explicit `null` clears it. Collapse the two and there is no way to
    /// unassign a card or lift a block at all — every "clear" silently becomes
    /// "leave it", and a blocked card can never be unblocked from the phone.
    #[tokio::test]
    async fn a_card_patch_tells_leave_it_apart_from_clear_it() {
        let client = RecordingClient::new(ISSUE_RESPONSE);
        patch_issue(
            &client,
            "p1".to_string(),
            12,
            crate::api::IssuePatch {
                title: None,
                description: None,
                attachments: None,
                priority: None,
                // Left alone: the key must not appear at all.
                assignee: crate::api::StringPatch::Keep,
                // Lifted: an explicit null, which is the unblock door.
                blocked_reason: crate::api::StringPatch::Clear,
                cancelled: None,
                parent: None,
                stage: None,
                pinned: Some(true),
            },
        )
        .await
        .expect("patch");

        let call = client.only_call();
        assert_eq!(call.method, "PATCH");
        assert_eq!(call.path, "/v1/projects/p1/issues/12");
        assert_eq!(call.body, r#"{"blocked_reason":null,"pinned":true}"#);
    }

    /// The other half: setting a value writes it, and the card comes back
    /// decoded — including the badge the board reads to say a run on this card
    /// is waiting for an answer.
    #[tokio::test]
    async fn a_card_patch_sets_a_value_and_decodes_the_card_it_answers_with() {
        let client = RecordingClient::new(ISSUE_RESPONSE);
        let issue = patch_issue(
            &client,
            "p1".to_string(),
            12,
            crate::api::IssuePatch {
                title: None,
                description: None,
                attachments: None,
                priority: Some(IssuePriority::Urgent),
                assignee: crate::api::StringPatch::Set {
                    value: "agent-1".to_string(),
                },
                blocked_reason: crate::api::StringPatch::Keep,
                cancelled: None,
                parent: None,
                stage: None,
                pinned: None,
            },
        )
        .await
        .expect("patch");

        assert_eq!(
            client.only_call().body,
            r#"{"priority":"urgent","assignee":"agent-1"}"#
        );
        assert_eq!(issue.number, 12);
        assert!(matches!(issue.status, IssueStatus::InProgress));
        assert!(matches!(issue.priority, IssuePriority::High));
        assert!(issue.approval_pending);
    }

    /// A move carries the destination column's WHOLE order, this card in it —
    /// the gateway renumbers the column in one transaction, so a body that
    /// named only the moved card would renumber the column to just that card.
    #[tokio::test]
    async fn a_move_carries_the_destination_columns_whole_order() {
        let client = RecordingClient::new(ISSUE_RESPONSE);
        move_issue(
            &client,
            "p1".to_string(),
            12,
            IssueStatus::InProgress,
            vec![9, 12, 4],
        )
        .await
        .expect("move");

        let call = client.only_call();
        assert_eq!(call.method, "POST");
        assert_eq!(call.path, "/v1/projects/p1/issues/12/move");
        assert_eq!(
            call.body,
            r#"{"status":"in_progress","ordered_numbers":[9,12,4]}"#
        );
    }

    /// A status this build cannot name never reaches the wire: asking the
    /// server to move a card into a column we have no word for is a request
    /// nobody can honour, and it must fail before the request, not after.
    #[tokio::test]
    async fn a_move_to_a_status_this_build_cannot_name_is_refused_locally() {
        let client = RecordingClient::new(ISSUE_RESPONSE);
        let err = move_issue(
            &client,
            "p1".to_string(),
            12,
            IssueStatus::Unknown,
            vec![12],
        )
        .await
        .expect_err("an unknown status cannot be sent");

        assert!(err.contains("unknown status"), "{err}");
        assert!(
            client.calls.lock().is_empty(),
            "nothing may reach the gateway"
        );
    }

    /// A card whose gateway grew a status still decodes: the arm costs that
    /// one card its word, where a hard failure would blank the whole board.
    #[tokio::test]
    async fn a_card_with_a_status_this_build_cannot_name_still_decodes() {
        let client = RecordingClient::new(
            r#"{"items":[{"number":1,"project_id":"p1","title":"t","description":"","status":"swimlane","priority":"whenever","position":0,"pinned":false,"stage":0,"unread":0,"last_run_failed":false,"opened_by_agent":false,"created_at_ms":1,"updated_at_ms":2}]}"#,
        );
        let issues = list_project_issues(&client, "p1".to_string())
            .await
            .expect("list");

        assert_eq!(issues.len(), 1);
        assert!(matches!(issues[0].status, IssueStatus::Unknown));
        assert!(matches!(issues[0].priority, IssuePriority::Unknown));
        // The field predates this gateway: absent reads as nothing waiting.
        assert!(!issues[0].approval_pending);
    }

    /// Two answers, and the `call_id` rides the path rather than the body —
    /// the queue is keyed by it.
    #[tokio::test]
    async fn answering_a_card_prompt_names_the_call_in_the_path() {
        let client = RecordingClient::empty();
        resolve_issue_approval(
            &client,
            "p1".to_string(),
            12,
            "call-7".to_string(),
            crate::api::IssueApprovalDecision::Deny,
        )
        .await
        .expect("resolve");

        let call = client.only_call();
        assert_eq!(call.path, "/v1/projects/p1/issues/12/approvals/call-7");
        assert_eq!(call.body, r#"{"decision":"deny"}"#);
    }

    /// A run's transcript hangs off the board, the card and the attempt — not
    /// off a session id — and still answers the same page frame the chat
    /// transcript renders.
    #[tokio::test]
    async fn a_run_transcript_page_is_addressed_by_card_and_attempt() {
        let client = RecordingClient::new(
            r#"{"transcript":[{"id":"r1"}],"has_more":true,"oldest_ordinal":4,"newest_ordinal":9}"#,
        );
        let frame = fetch_run_transcript_page(&client, "p1".to_string(), 12, 3, Some(9), Some(50))
            .await
            .expect("transcript");

        assert_eq!(
            client.only_call().path,
            "/v1/projects/p1/issues/12/runs/3/transcript?before_ordinal=9&limit=50"
        );
        assert_eq!(
            frame,
            r#"{"kind":"history_page","rows":[{"id":"r1"}],"oldest_ordinal":4,"newest_ordinal":9,"has_more":true}"#
        );
    }

    /// Setting a face and clearing one are the same door, and the difference
    /// is whether `blob_id` is THERE: the gateway reads an absent field as
    /// "clear", so a clear that sent `null` and a clear that sent nothing must
    /// not be two behaviours here.
    #[tokio::test]
    async fn an_avatar_is_set_by_blob_and_cleared_by_absence() {
        let client = RecordingClient::empty();
        set_agent_avatar(&client, "a-dev".into(), Some("sha256:aa.tok".into()))
            .await
            .expect("set avatar");
        let call = client.only_call();
        assert_eq!(call.method, "PUT");
        assert_eq!(call.path, "/v1/agents/a-dev/avatar");
        assert!(
            call.body.contains("\"blob_id\":\"sha256:aa.tok\""),
            "{}",
            call.body
        );

        let client = RecordingClient::empty();
        set_agent_avatar(&client, "a-dev".into(), None)
            .await
            .expect("clear avatar");
        assert_eq!(client.only_call().body, "{}");
    }

    /// The board's settings are a FULL REPLACE, and `agents_may_merge` is the
    /// field where that is dangerous: it is a plain `bool` with no "unset", and
    /// the gateway defaults a missing one to `false`. So an omission is not
    /// "leave it alone" — it silently turns a board's merging off, which is
    /// what every Save from this app did until this field existed. Both
    /// directions are pinned, because a body that always said `true` would
    /// pass a test that only checked the interesting one.
    #[tokio::test]
    async fn the_settings_body_always_states_whether_the_board_merges() {
        for merges in [true, false] {
            let client = RecordingClient::empty();
            update_project(
                &client,
                "p-1".into(),
                crate::api::ProjectSettings {
                    name: "rglide".into(),
                    description: String::new(),
                    daily_budget_micros: None,
                    daily_budget_tokens: None,
                    max_parallel_issue_runs: None,
                    agents_may_merge: merges,
                },
            )
            .await
            .expect("update project");
            let call = client.only_call();
            assert_eq!(call.method, "PUT");
            assert_eq!(call.path, "/v1/projects/p-1");
            assert!(
                call.body
                    .contains(&format!("\"agents_may_merge\":{merges}")),
                "{}",
                call.body
            );
        }
    }

    /// An id reaches a URL, so it goes through the same gate every other path
    /// segment on this client does: a separator is REFUSED rather than
    /// encoded (an id that could carry one could address another route), and
    /// what is merely awkward is encoded.
    #[tokio::test]
    async fn an_avatar_id_is_gated_like_any_path_segment() {
        let client = RecordingClient::empty();
        assert!(
            set_agent_avatar(&client, "a-dev/../sessions".into(), None)
                .await
                .is_err()
        );

        set_agent_avatar(&client, "a dev".into(), None)
            .await
            .expect("set avatar");
        assert_eq!(client.only_call().path, "/v1/agents/a%20dev/avatar");
    }

    /// The pin is replaced WHOLE, so every level is sent — including the ones
    /// that are `null`. An encoder that skipped them would leave two thirds of
    /// a pin pointing at an entry the agent no longer uses, and clearing would
    /// be unreachable.
    #[tokio::test]
    async fn an_agent_model_pin_sends_every_level_including_the_cleared_ones() {
        let client = RecordingClient::empty();
        set_agent_model(
            &client,
            "a-dev".into(),
            Some("claude".into()),
            Some("claude-sonnet-5".into()),
            None,
        )
        .await
        .expect("set model");
        let call = client.only_call();
        assert_eq!(call.method, "PUT");
        assert_eq!(call.path, "/v1/agents/a-dev/model");
        assert!(call.body.contains("\"llm\":\"claude\""), "{}", call.body);
        assert!(
            call.body.contains("\"model\":\"claude-sonnet-5\""),
            "{}",
            call.body
        );
        assert!(
            call.body.contains("\"reasoning_effort\":null"),
            "an absent level must be sent as null, not omitted: {}",
            call.body
        );
    }

    /// Clearing is all three at once. Anything less leaves a partial pin.
    #[tokio::test]
    async fn clearing_a_pin_sends_three_nulls() {
        let client = RecordingClient::empty();
        set_agent_model(&client, "a-dev".into(), None, None, None)
            .await
            .expect("clear model");
        let call = client.only_call();
        assert_eq!(
            call.body, r#"{"llm":null,"model":null,"reasoning_effort":null}"#,
            "clearing must name every level"
        );
    }

    /// An agent id reaches the PATH, so a traversal attempt is refused rather
    /// than encoded — the same gate every other path segment goes through.
    #[tokio::test]
    async fn an_agent_id_may_not_name_a_path() {
        let client = RecordingClient::empty();
        let err = set_agent_model(&client, "../admin".into(), None, None, None)
            .await
            .expect_err("a traversal must be refused");
        assert!(err.contains("agent_id"), "{err}");
    }
}
