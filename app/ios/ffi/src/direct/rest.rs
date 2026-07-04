//! Admin-Bearer REST calls the direct transport needs: mint a chat session
//! (and its channel token), rotate a dead channel token, list the gateway's
//! chat sessions, and refetch a transcript slice after a `Frame::Reset`. All
//! authenticate with the stored admin Bearer token; the minted channel token
//! authorizes the WS + blobs.

use serde::Deserialize;

use super::INVALID_TOKEN_CODE;

/// `POST /v1/chat/sessions` (and `.../{id}/token`) response. The gateway also
/// returns `channel_token_header`, but we know it (`CHANNEL_TOKEN_HEADER`), so we
/// don't decode it.
#[derive(Deserialize)]
pub(super) struct ChatSessionCredential {
    pub(super) session_id: String,
    pub(super) channel_token: String,
}

/// Mint a fresh chat session + channel token (`POST /v1/chat/sessions`, empty body).
pub(super) async fn mint_session(
    base: &str,
    admin_token: &str,
) -> Result<ChatSessionCredential, String> {
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/sessions"))
        .bearer_auth(admin_token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|e| format!("could not reach Baybo: {e}"))?;
    parse_credential(resp).await
}

/// Mint a fresh channel token for an existing session (`POST /v1/chat/sessions/{id}
/// /token`). The prior token is NOT revoked immediately — the gateway keys tokens
/// by token string (not session), so the old one lingers until its WS closes or
/// the gateway's TTL janitor reaps it. Used when the live token is rejected, or
/// after a relaunch left only the session id.
pub(super) async fn rotate_token(
    base: &str,
    admin_token: &str,
    session_id: &str,
) -> Result<ChatSessionCredential, String> {
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/chat/sessions/{session_id}/token"))
        .bearer_auth(admin_token)
        .send()
        .await
        .map_err(|e| format!("could not reach Baybo: {e}"))?;
    parse_credential(resp).await
}

async fn parse_credential(resp: reqwest::Response) -> Result<ChatSessionCredential, String> {
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

/// List the gateway's chat sessions (`GET /v1/chat/sessions`, hidden + cron
/// filtered by the gateway's defaults).
pub(super) async fn list_sessions(
    base: &str,
    admin_token: &str,
) -> Result<Vec<SessionSummary>, String> {
    let resp = reqwest::Client::new()
        .get(format!("{base}/v1/chat/sessions"))
        .bearer_auth(admin_token)
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
