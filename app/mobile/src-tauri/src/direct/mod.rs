//! Direct (non-relay) gateway access — the web-dashboard style.
//!
//! [`login`]/[`status`]/[`logout`] (this file) validate + persist the gateway
//! base URL + admin Bearer token. The chat transport ([`chat`]) then mints a
//! narrower **channel token** over REST ([`rest`]) and speaks the raw-MessagePack
//! `/v1/channel-ws` protocol — the same one `app/web` uses — while attachments
//! ([`blob`]) go over plain `/v1/blobs`. This whole path deliberately bypasses
//! the relay + Noise E2E design the scan-to-pair flow uses (see `pairing.rs`).
//!
//! Two credentials, never conflated: the **admin Bearer** token (stored here,
//! authorizes the REST surface) and the minted **channel token** (authorizes the
//! WS + blobs, held live in [`chat::DirectSessions`]).

mod blob;
mod chat;
mod push;
mod rest;

pub use blob::{image_data, upload_bytes};
pub use chat::{DirectSessions, forget, session_create};
pub use push::register as register_push;

use serde::{Deserialize, Serialize};

use crate::keychain;

/// The header the gateway reads the minted channel token from on `/v1/channel-ws`
/// and `/v1/blobs` (mirrors the gateway's `auth/token.rs` constant). The admin
/// Bearer token is NEVER sent here — these endpoints reject it.
pub(crate) const CHANNEL_TOKEN_HEADER: &str = "x-baybo-channel-token";

/// Stable 401 discriminator returned to the webview (matched in `directConnect`
/// in App.tsx to show a localized "invalid token" message). A code, not prose,
/// so reworded errors can't silently change the cross-language contract.
pub const INVALID_TOKEN_CODE: &str = "invalid_token";

/// Persisted direct-connection credentials.
#[derive(Serialize, Deserialize)]
pub(crate) struct DirectCredentials {
    pub(crate) base_url: String,
    pub(crate) token: String,
}

/// What the UI needs about the current direct connection (never the token).
#[derive(Serialize)]
pub struct DirectStatus {
    pub base_url: String,
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
pub async fn login(base_url: String, token: String) -> Result<DirectStatus, String> {
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

    Ok(DirectStatus { base_url: base })
}

/// The current direct connection (base URL only), if credentials are persisted.
pub fn status() -> Result<Option<DirectStatus>, String> {
    Ok(credentials()?.map(|c| DirectStatus {
        base_url: c.base_url,
    }))
}

/// Forget the direct-connection credentials.
pub fn logout() -> Result<(), String> {
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
/// path `/v1/channel-ws`. The channel token rides a request header (set by the
/// caller), not the query string — a native client can read a 401 directly.
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
