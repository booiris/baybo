//! Typed gateway API calls shared by direct REST and relay API-tunnel transports.

use std::future::Future;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::api::{
    ChatSessionSummary, CronJobStatus, CronJobSummary, DeckCardInfo, DeckLayoutEntryInput,
    DeckSnapshotInfo, DeckView, LlmModelCatalog, LlmModelInfo, SessionModelPin,
};

const PATH_CHAT_SESSIONS: &str = "/v1/chat/sessions";
const PATH_CRON: &str = "/v1/cron";
const PATH_LLM_MODELS: &str = "/v1/llm/models";
const PATH_DECK: &str = "/v1/deck";
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
    validate_path_segment(&session_id, "session_id")?;
    let mut path = format!("{PATH_CHAT_SESSIONS}/{session_id}");
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
    validate_path_segment(&session_id, "session_id")?;
    let mut path = format!("{PATH_CHAT_SESSIONS}/{session_id}/sync");
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
        percent_encode_query(platform_msg_id)
    );
    client.get_json(&path).await
}

/// Percent-encode a query value (everything outside RFC 3986 unreserved).
/// `platform_msg_id`s are native-minted UUIDs today, but a retry payload
/// round-trips through the webview — encode defensively.
fn percent_encode_query(value: &str) -> String {
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

pub(crate) async fn upload_bytes<C: GatewayBlobClient + Sync>(
    client: &C,
    bytes: Vec<u8>,
    mime_type: String,
    deck_card: Option<String>,
) -> Result<String, String> {
    client.upload_blob(bytes, mime_type, deck_card).await
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
                {"name":"5.5-max","provider":"openai","model":"gpt-5.5","model_list":[{"model":"gpt-5.5"},{"model":"o3","context_window":200000}],"reasoning_effort":"xhigh","api_key_configured":false,"is_default":false,"effective_context_window":400000,"effective_supports_vision":false,"effective_pricing":{}}
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
}
