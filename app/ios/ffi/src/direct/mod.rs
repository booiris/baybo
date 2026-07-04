//! Direct (non-relay) gateway access — the web-dashboard style.
//!
//! [`login`]/[`status`]/[`logout`] (this file) validate + persist the gateway
//! base URL + admin Bearer token. The chat transport ([`chat`]) uses that same
//! Bearer to speak the raw-MessagePack `/v1/channel-ws` protocol — the same one
//! `app/web` uses — while attachments ([`blob`]) go over plain `/v1/blobs`.
//! This whole path deliberately bypasses the relay + Noise E2E design the
//! scan-to-pair flow uses (see `pairing.rs`).

mod blob;
mod chat;
mod push;
mod rest;

pub(crate) use blob::{image_data, upload_bytes};
pub(crate) use chat::{DirectSessions, forget, session_create};
pub(crate) use push::register as register_push;

use serde::{Deserialize, Serialize};

use crate::api::ChatSessionSummary;
use crate::keychain;

/// Stable 401 discriminator, folded into `BayboError::InvalidToken` at the FFI
/// boundary. A code, not prose, so reworded errors can't silently change the
/// contract.
pub(crate) const INVALID_TOKEN_CODE: &str = "invalid_token";

/// Persisted direct-connection credentials. Serialized to JSON in the keychain;
/// the byte format is the upgrade-continuity contract with installs made by the
/// Tauri shell, so field names must not change.
#[derive(Serialize, Deserialize)]
pub(crate) struct DirectCredentials {
    pub(crate) base_url: String,
    pub(crate) token: String,
}

/// Normalize the typed address: trim, lowercase the scheme, default to `https://`
/// when none is given (secure by default), then drop a trailing slash. The scheme
/// is resolved BEFORE stripping slashes so a bare `http://` isn't mangled into a
/// hostname, and lowercased so the case-sensitive checks in [`login`] /
/// [`channel_ws_url`] accept `HTTPS://…`. An explicit `http://` is preserved — the
/// user opting into an un-TLS'd local Baybo (cleartext; no E2E on this path).
fn normalize_base(base_url: &str) -> String {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let with_scheme = match trimmed.find("://") {
        Some(idx) => {
            let (scheme, rest) = trimmed.split_at(idx);
            format!("{}{}", scheme.to_ascii_lowercase(), rest)
        }
        None => format!("https://{trimmed}"),
    };
    with_scheme.trim_end_matches('/').to_string()
}

/// Validate `base_url` + `token` against `GET /v1/status`, then persist them.
/// Returns the normalized base URL.
pub(crate) async fn login(base_url: String, token: String) -> Result<String, String> {
    let base = normalize_base(&base_url);
    if base.is_empty() {
        return Err("enter the Baybo address".into());
    }
    if !base.starts_with("https://") && !base.starts_with("http://") {
        return Err("the address must start with http:// or https://".into());
    }
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err("enter the access token".into());
    }

    let resp = reqwest::Client::new()
        .get(format!("{base}/v1/status"))
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| format!("could not reach Baybo: {e}"))?;

    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(INVALID_TOKEN_CODE.into());
    }
    if !resp.status().is_success() {
        return Err(format!("Baybo returned HTTP {}", resp.status().as_u16()));
    }

    let bytes = serde_json::to_vec(&DirectCredentials {
        base_url: base.clone(),
        token,
    })
    .map_err(|e| e.to_string())?;
    // Record which bind happened last BEFORE committing the credential record, so
    // the resolver breaks a transient both-present tie (a hiccuped supersede below)
    // toward direct — the binding just made. Ordered first on purpose: a failed
    // marker write must not leave the record committed under a stale marker (which
    // would resolve the tie to the superseded leg). A marker with the record absent
    // is harmless — active_leg only consults it when both credentials exist.
    keychain::store_active_binding(crate::binding::DIRECT_MARKER)?;
    keychain::store_direct_credentials(&bytes)?;
    // One app binds one Baybo: a fresh direct login supersedes any relay pairing
    // so a later disconnect can't resurrect a stale binding. Best-effort —
    // don't fail the successful login if the (idempotent) cleanup hiccups; the
    // marker above keeps resolution correct even if this leaves the relay record.
    let _ = crate::relay::forget_pairing();

    Ok(base)
}

/// The current direct connection's base URL, if credentials are persisted.
pub(crate) fn status() -> Result<Option<String>, String> {
    Ok(credentials()?.map(|c| c.base_url))
}

/// List the gateway's chat sessions over REST (the web sidebar's list, hidden +
/// cron filtered). Direct-only: the relay wire protocol has no list frame, so
/// the app renders its device-local registry there instead.
pub(crate) async fn sessions_list() -> Result<Vec<ChatSessionSummary>, String> {
    let creds = credentials()?.ok_or("not connected; sign in first")?;
    let items = rest::list_sessions(&creds.base_url, &creds.token).await?;
    Ok(items
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

/// Forget the direct-connection credentials.
pub(crate) fn logout() -> Result<(), String> {
    keychain::delete_direct_credentials()
}

/// Whether a direct login is persisted — the `direct` arm of the active-binding
/// resolver ([`crate::binding`]). Cheaper than [`credentials`]: it never decodes
/// the record, just checks the keychain slot is populated.
pub(crate) fn has_credentials() -> Result<bool, String> {
    Ok(keychain::read_direct_credentials()?.is_some())
}

/// Read the stored direct credentials (base URL + admin token). `Ok(None)` = not
/// connected. The REST/WS/blob legs read `base_url` from here, and REST reads the
/// admin `token`.
pub(crate) fn credentials() -> Result<Option<DirectCredentials>, String> {
    let Some(bytes) = keychain::read_direct_credentials()? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| e.to_string())
}

/// Build the `/v1/channel-ws` URL from a stored base URL: `http→ws`, `https→wss`,
/// path `/v1/channel-ws`. The admin Bearer rides the upgrade request's
/// `Authorization` header so the client can read a 401 directly.
pub(crate) fn channel_ws_url(base_url: &str) -> Result<String, String> {
    let rest = base_url
        .strip_prefix("https://")
        .map(|r| ("wss://", r))
        .or_else(|| base_url.strip_prefix("http://").map(|r| ("ws://", r)));
    match rest {
        Some((scheme, host)) => Ok(format!("{scheme}{host}/v1/channel-ws")),
        None => Err("stored Baybo address has no http(s) scheme".into()),
    }
}
