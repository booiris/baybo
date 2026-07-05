//! Typed gateway API calls shared by direct REST and relay API-tunnel transports.

use std::future::Future;

use serde::{Deserialize, Serialize};
use serde::de::DeserializeOwned;

use crate::api::ChatSessionSummary;

const PATH_CHAT_SESSIONS: &str = "/v1/chat/sessions";
const PATH_MOBILE_APNS_TOKEN: &str = "/v1/mobile/apns-token";
pub(crate) const PATH_BLOBS: &str = "/v1/blobs";
const EMPTY_JSON_OBJECT: &[u8] = b"{}";
const CATCH_UP_LIMIT: u32 = 200;

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
    pinned: bool,
}

#[derive(Deserialize)]
struct ChatSessionDetail {
    transcript: Vec<ChatTranscriptItem>,
    has_more: bool,
    #[serde(default)]
    oldest_ordinal: Option<i64>,
    #[serde(default)]
    newest_ordinal: Option<i64>,
}

#[derive(Deserialize)]
struct ChatTranscriptItem {
    ordinal: i64,
    kind: String,
    role: String,
    text: String,
    #[serde(default)]
    attachments: Vec<HistoryAttachment>,
}

#[derive(Clone, Deserialize, Serialize)]
struct HistoryAttachment {
    kind: String,
    blob_id: String,
    mime_type: String,
    size: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filename: Option<String>,
}

#[derive(Serialize)]
struct HistoryPageFrame {
    kind: &'static str,
    messages: Vec<HistoryMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    oldest_ordinal: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    newest_ordinal: Option<i64>,
    has_more: bool,
}

#[derive(Deserialize)]
struct ChatCatchUpResponse {
    items: Vec<serde_json::Value>,
    #[serde(default)]
    newest_ordinal: Option<i64>,
    truncated: bool,
}

#[derive(Serialize)]
struct CatchUpFrame {
    kind: &'static str,
    items: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    newest_ordinal: Option<i64>,
    truncated: bool,
}

#[derive(Serialize)]
struct HistoryMessage {
    content: String,
    role: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    platform_msg_id: String,
    ordinal: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    attachments: Vec<HistoryAttachment>,
}

#[derive(Serialize)]
struct UpdateApnsTokenRequest<'a> {
    apns_token: &'a str,
    apns_env: &'a str,
}

pub(crate) async fn create_session<C: GatewayJsonClient + Sync>(
    client: &C,
) -> Result<String, String> {
    let created: ChatSessionCreated = client
        .post_json(PATH_CHAT_SESSIONS, EMPTY_JSON_OBJECT.to_vec())
        .await?;
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
            pinned: s.pinned,
        })
        .collect())
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
    let messages = detail
        .transcript
        .into_iter()
        .filter_map(|item| {
            if item.kind != "message" {
                return None;
            }
            if item.ordinal < 0 {
                return None;
            }
            if item.role != "user" && item.role != "assistant" {
                return None;
            }
            Some(HistoryMessage {
                content: item.text,
                role: item.role,
                platform_msg_id: String::new(),
                ordinal: item.ordinal,
                attachments: item.attachments,
            })
        })
        .collect();
    let page = HistoryPageFrame {
        kind: "history_page",
        messages,
        oldest_ordinal: detail.oldest_ordinal,
        newest_ordinal: detail.newest_ordinal,
        has_more: detail.has_more,
    };
    serde_json::to_string(&page).map_err(|e| format!("encode history page: {e}"))
}

pub(crate) async fn fetch_catch_up<C: GatewayJsonClient + Sync>(
    client: &C,
    session_id: String,
    since_ordinal: i64,
) -> Result<String, String> {
    validate_path_segment(&session_id, "session_id")?;
    let mut path = format!("{PATH_CHAT_SESSIONS}/{session_id}/catch-up");
    let mut first_query = true;
    append_query(
        &mut path,
        &mut first_query,
        "since_ordinal",
        since_ordinal,
    );
    append_query(&mut path, &mut first_query, "limit", CATCH_UP_LIMIT);
    let response: ChatCatchUpResponse = client.get_json(&path).await?;
    let frame = CatchUpFrame {
        kind: "catch_up",
        items: response.items,
        newest_ordinal: response.newest_ordinal,
        truncated: response.truncated,
    };
    serde_json::to_string(&frame).map_err(|e| format!("encode catch-up page: {e}"))
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
