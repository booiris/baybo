//! Compatibility for OAuth bundles minted before Baybo dynamic registration.
use super::oauth::{ISSUER, RefreshError};
use super::token_bundle::OAuthTokenBundle;
use serde::Deserialize;
use tracing::{info, warn};
use url::form_urlencoded;

pub(super) const LEGACY_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub(super) const LEGACY_ORIGINATOR: &str = "codex_cli_rs";
const USER_AGENT: &str = concat!("baybo/", env!("CARGO_PKG_VERSION"));
const ERROR_BODY_TRUNCATE: usize = 512;

fn truncate_body(body: &str) -> String {
    body.trim().chars().take(ERROR_BODY_TRUNCATE).collect()
}

#[derive(Deserialize)]
struct TokenEndpointResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

pub(super) async fn refresh_at(
    issuer: &str,
    refresh_token: &str,
    client: &reqwest::Client,
) -> std::result::Result<OAuthTokenBundle, RefreshError> {
    let endpoint = format!("{issuer}/oauth/token");
    let body = form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", LEGACY_CLIENT_ID)
        .append_pair("grant_type", "refresh_token")
        .append_pair("refresh_token", refresh_token)
        .finish();
    let resp = client
        .post(&endpoint)
        .header("originator", LEGACY_ORIGINATOR)
        .header("User-Agent", USER_AGENT)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| RefreshError::Transient(e.to_string()))?;

    let status = resp.status();
    let body_text = resp
        .text()
        .await
        .map_err(|e| RefreshError::Transient(format!("refresh response read: {e}")))?;
    if status.is_success() {
        let body: TokenEndpointResponse = serde_json::from_str(&body_text).map_err(|e| {
            RefreshError::Transient(format!(
                "refresh response parse: {e}; body: {}",
                truncate_body(&body_text)
            ))
        })?;
        // Server may rotate the refresh_token; keep the old one if it didn't.
        let next_refresh = body
            .refresh_token
            .unwrap_or_else(|| refresh_token.to_string());
        let access = body.access_token.ok_or_else(|| {
            RefreshError::Transient("refresh response missing access_token".into())
        })?;
        let id = body
            .id_token
            .ok_or_else(|| RefreshError::Transient("refresh response missing id_token".into()))?;
        let bundle = OAuthTokenBundle::from_token_response(access, next_refresh, id)
            .map_err(|e| RefreshError::Transient(format!("bundle build: {e}")))?;
        info!(
            event = "openai_subscription_token_refresh",
            outcome = "success",
            "refreshed openai-subscription token"
        );
        return Ok(bundle);
    }

    let body_snippet = truncate_body(&body_text);
    if status == reqwest::StatusCode::UNAUTHORIZED {
        warn!(
            event = "openai_subscription_token_refresh",
            outcome = "permanent",
            "refresh permanently failed: {body_snippet}"
        );
        return Err(RefreshError::Permanent(body_snippet));
    }
    warn!(
        event = "openai_subscription_token_refresh",
        outcome = "transient",
        %status,
        "refresh transiently failed"
    );
    Err(RefreshError::Transient(format!(
        "status={status}: {body_snippet}"
    )))
}

pub(super) async fn revoke(refresh_token: &str, client: &reqwest::Client) -> std::io::Result<()> {
    let endpoint = format!("{ISSUER}/oauth/revoke");
    let body = form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", LEGACY_CLIENT_ID)
        .append_pair("token", refresh_token)
        .append_pair("token_type_hint", "refresh_token")
        .finish();
    let resp = client
        .post(&endpoint)
        .header("originator", LEGACY_ORIGINATOR)
        .header("User-Agent", USER_AGENT)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(std::io::Error::other)?;
    let status = resp.status();
    if status.is_success() {
        info!(
            event = "openai_subscription_token_revoke",
            outcome = "success",
            "openai-subscription: refresh token revoked server-side"
        );
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        warn!(
            event = "openai_subscription_token_revoke",
            outcome = "non_success_status",
            %status,
            "openai-subscription: revoke returned non-success status: {body}"
        );
        Err(std::io::Error::other(format!(
            "revoke status {status}: {body}"
        )))
    }
}
