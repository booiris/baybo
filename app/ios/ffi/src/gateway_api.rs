//! Typed gateway API calls shared by direct REST and relay API-tunnel transports.

use std::future::Future;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::api::ChatSessionSummary;

const PATH_CHAT_SESSIONS: &str = "/v1/chat/sessions";
pub(crate) const PATH_BLOBS: &str = "/v1/blobs";
const EMPTY_JSON_OBJECT: &[u8] = b"{}";

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
