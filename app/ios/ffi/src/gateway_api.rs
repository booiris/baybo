//! Typed gateway API calls shared by direct REST and relay API-tunnel transports.

use std::future::Future;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::api::ChatSessionSummary;

const PATH_CHAT_SESSIONS: &str = "/v1/chat/sessions";
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
}

pub(crate) trait GatewayBlobClient {
    fn upload_blob(
        &self,
        bytes: Vec<u8>,
        mime_type: String,
    ) -> impl Future<Output = Result<String, String>> + Send + '_;

    fn download_blob(
        &self,
        blob_id: String,
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
        })
        .collect())
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

pub(crate) async fn hide_session<C: GatewayJsonClient + Sync>(
    client: &C,
    session_id: String,
) -> Result<(), String> {
    validate_path_segment(&session_id, "session_id")?;
    let path = format!("{PATH_CHAT_SESSIONS}/{session_id}");
    client.delete_empty(&path).await
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
) -> Result<String, String> {
    client.upload_blob(bytes, mime_type).await
}

pub(crate) async fn download_blob_bytes<C: GatewayBlobClient + Sync>(
    client: &C,
    blob_id: String,
) -> Result<Vec<u8>, String> {
    client.download_blob(blob_id).await
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
