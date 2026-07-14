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
            cron_job_id: s.cron_job_id,
            cron_job_title: s.cron_job_title,
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
    }

    #[tokio::test]
    async fn list_sessions_carries_every_row_field_through() {
        let client = RecordingClient::new(
            r#"{"items":[{"session_id":"s1","created_at":"c","last_active":"l","last_user_text":"hi","last_message_text":"reply","title":"A chat","pinned":false,"archived":true,"unread_count":3,"cron_job_id":"cj-1","cron_job_title":"Morning brief"}]}"#,
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
