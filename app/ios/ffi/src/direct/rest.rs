//! Bearer REST calls the direct transport needs: create a chat session and list
//! the gateway's device chat sessions. The stored gateway access token plus device
//! id header also authorizes the direct WS + blob paths on the admin listener.

use serde::Deserialize;

use super::INVALID_TOKEN_CODE;

/// `POST /v1/chat/sessions` response.
#[derive(Deserialize)]
pub(super) struct ChatSessionCreated {
    pub(super) session_id: String,
}

/// Create a fresh chat session (`POST /v1/chat/sessions`, empty body).
pub(super) async fn create_session(
    base: &str,
    access_token: &str,
) -> Result<ChatSessionCreated, String> {
    let device_id = super::device_id()?;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/sessions"))
        .bearer_auth(access_token)
        .header(super::DEVICE_ID_HEADER, device_id)
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|e| format!("could not reach Baybo: {e}"))?;
    parse_created(resp).await
}

async fn parse_created(resp: reqwest::Response) -> Result<ChatSessionCreated, String> {
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(INVALID_TOKEN_CODE.into());
    }
    if !resp.status().is_success() {
        return Err(format!("Baybo returned HTTP {}", resp.status().as_u16()));
    }
    resp.json()
        .await
        .map_err(|e| format!("decode session: {e}"))
}

/// `GET /v1/chat/sessions` response envelope (the web sidebar's list). The
/// default query already filters hidden and cron sessions, which is exactly
/// the chat-list view the app wants.
#[derive(Deserialize)]
struct ChatSessionsList {
    items: Vec<SessionSummary>,
}

/// One session row from the gateway, newest first. Timestamps stay RFC 3339
/// strings across the FFI; Swift parses them for the age label.
#[derive(Deserialize)]
pub(super) struct SessionSummary {
    pub(super) session_id: String,
    pub(super) created_at: String,
    pub(super) last_active: String,
    #[serde(default)]
    pub(super) last_user_text: Option<String>,
    pub(super) pinned: bool,
}

/// List the gateway's device chat sessions (`GET /v1/chat/sessions`, hidden + cron
/// filtered by the gateway's defaults).
pub(super) async fn list_sessions(
    base: &str,
    access_token: &str,
) -> Result<Vec<SessionSummary>, String> {
    let device_id = super::device_id()?;
    let resp = reqwest::Client::new()
        .get(format!("{base}/v1/chat/sessions"))
        .bearer_auth(access_token)
        .header(super::DEVICE_ID_HEADER, device_id)
        .send()
        .await
        .map_err(|e| format!("could not reach Baybo: {e}"))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(INVALID_TOKEN_CODE.into());
    }
    if !resp.status().is_success() {
        return Err(format!("Baybo returned HTTP {}", resp.status().as_u16()));
    }
    let list: ChatSessionsList = resp
        .json()
        .await
        .map_err(|e| format!("decode sessions: {e}"))?;
    Ok(list.items)
}
