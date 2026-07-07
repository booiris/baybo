//! Direct (non-relay) gateway access — the web-dashboard style.
//!
//! [`login`]/[`status`]/[`logout`] (this file) validate + persist the gateway
//! base URL + gateway access token. The chat transport ([`chat`]) uses that same
//! Bearer plus a stable device id header to speak the raw-MessagePack
//! `/v1/channel-ws` protocol. HTTP-shaped chat metadata and blob calls reuse
//! one authenticated client via [`crate::gateway_api`].
//! This whole path deliberately bypasses the relay + Noise E2E design the
//! scan-to-pair flow uses (see `pairing.rs`).

mod blob;
mod chat;
mod push;

pub(crate) use blob::download_blob_bytes;
pub(crate) use chat::forget;
pub(crate) use push::register as register_push;

use std::time::Duration;

use parking_lot::Mutex;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::gateway_api::GatewayJsonClient;
use crate::keychain;
use crate::transport::SessionRegistry;

/// Stable 401 discriminator, folded into `BayboError::InvalidToken` at the FFI
/// boundary. A code, not prose, so reworded errors can't silently change the
/// contract.
pub(crate) const INVALID_TOKEN_CODE: &str = "invalid_token";

/// TCP/TLS connect budget for the shared REST/blob client: a black-holed
/// gateway address must fail the dial instead of pinning `login` (and the UI's
/// busy flag) open until the OS gives up. Matches the WS dial budget
/// (`transport::CONNECT_TIMEOUT`).
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Whole-request budget for the small JSON calls that gate UI state — the
/// login `/v1/status` probe and push params/register. Applied at those call
/// sites only, NOT on the client or in the `GatewayJsonClient` impl: blob
/// transfers and the transcript history/catch-up pages share the pool and may
/// legitimately outlive any sane fixed cap on a slow link.
const REST_REPLY_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) use device_proto::DEVICE_ID_HEADER;

/// Persisted direct-connection credentials. Serialized to JSON in the keychain;
/// the byte format is the upgrade-continuity contract with installs made by the
/// Tauri shell, so field names must not change.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct DirectCredentials {
    pub(crate) base_url: String,
    pub(crate) token: String,
}

/// Direct leg state: one global chat socket registry plus one authenticated HTTP
/// client whose pool/default headers are reused across REST, blob, and push calls.
pub(crate) struct DirectSessions {
    registry: SessionRegistry,
    http: Mutex<Option<DirectHttpCache>>,
}

impl Default for DirectSessions {
    fn default() -> Self {
        Self {
            registry: SessionRegistry::default(),
            http: Mutex::new(None),
        }
    }
}

#[derive(Clone)]
pub(crate) struct DirectHttp {
    base_url: String,
    device_id: String,
    client: reqwest::Client,
    headers: HeaderMap,
}

#[derive(Clone, PartialEq, Eq)]
struct DirectHttpKey {
    base_url: String,
    token: String,
    device_id: String,
}

#[derive(Clone)]
struct DirectHttpCache {
    key: DirectHttpKey,
    http: DirectHttp,
}

impl DirectSessions {
    pub(crate) fn chat_registry(&self) -> &SessionRegistry {
        &self.registry
    }

    pub(crate) fn http_client(&self) -> Result<DirectHttp, String> {
        let creds = credentials()?.ok_or("not connected; sign in first")?;
        let key = DirectHttpKey::new(creds, device_id()?);
        let mut guard = self.http.lock();
        if let Some(cached) = guard.as_ref()
            && cached.key == key
        {
            return Ok(cached.http.clone());
        }
        let cached = DirectHttpCache::new(key)?;
        let http = cached.http.clone();
        *guard = Some(cached);
        Ok(http)
    }

    fn set_http_client(&self, cached: DirectHttpCache) {
        *self.http.lock() = Some(cached);
    }

    pub(crate) fn clear_http_client(&self) {
        *self.http.lock() = None;
    }
}

impl DirectHttp {
    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    pub(crate) fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub(crate) fn device_id(&self) -> &str {
        &self.device_id
    }

    pub(crate) fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

#[allow(clippy::manual_async_fn)]
impl GatewayJsonClient for DirectHttp {
    fn get_json<'a, T>(
        &'a self,
        path: &'a str,
    ) -> impl std::future::Future<Output = Result<T, String>> + Send + 'a
    where
        T: DeserializeOwned + Send + 'static,
    {
        async move {
            let resp = self
                .client()
                .get(self.url(path))
                .send()
                .await
                .map_err(|e| format!("could not reach Baybo: {e}"))?;
            parse_json_response(resp).await
        }
    }

    fn post_json<'a, T>(
        &'a self,
        path: &'a str,
        body: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<T, String>> + Send + 'a
    where
        T: DeserializeOwned + Send + 'static,
    {
        async move {
            let resp = self
                .client()
                .post(self.url(path))
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await
                .map_err(|e| format!("could not reach Baybo: {e}"))?;
            parse_json_response(resp).await
        }
    }

    fn post_empty<'a>(
        &'a self,
        path: &'a str,
        body: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send + 'a {
        async move {
            let resp = self
                .client()
                .post(self.url(path))
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await
                .map_err(|e| format!("could not reach Baybo: {e}"))?;
            parse_empty_response(resp).await
        }
    }

    fn put_empty<'a>(
        &'a self,
        path: &'a str,
        body: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send + 'a {
        async move {
            let resp = self
                .client()
                .put(self.url(path))
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await
                .map_err(|e| format!("could not reach Baybo: {e}"))?;
            parse_empty_response(resp).await
        }
    }
}

async fn parse_json_response<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T, String> {
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(INVALID_TOKEN_CODE.into());
    }
    if !resp.status().is_success() {
        return Err(format!("Baybo returned HTTP {}", resp.status().as_u16()));
    }
    resp.json()
        .await
        .map_err(|e| format!("decode response: {e}"))
}

async fn parse_empty_response(resp: reqwest::Response) -> Result<(), String> {
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(INVALID_TOKEN_CODE.into());
    }
    if !resp.status().is_success() {
        return Err(format!("Baybo returned HTTP {}", resp.status().as_u16()));
    }
    Ok(())
}

impl DirectHttpCache {
    fn new(key: DirectHttpKey) -> Result<Self, String> {
        let mut headers = HeaderMap::new();
        let auth = HeaderValue::from_str(&format!("Bearer {}", key.token))
            .map_err(|e| format!("bad access token: {e}"))?;
        headers.insert(AUTHORIZATION, auth);
        let device =
            HeaderValue::from_str(&key.device_id).map_err(|e| format!("bad device id: {e}"))?;
        headers.insert(HeaderName::from_static(DEVICE_ID_HEADER), device);
        let client = reqwest::Client::builder()
            .default_headers(headers.clone())
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .build()
            .map_err(|e| format!("build direct HTTP client: {e}"))?;
        Ok(Self {
            http: DirectHttp {
                base_url: key.base_url.clone(),
                device_id: key.device_id.clone(),
                client,
                headers,
            },
            key,
        })
    }
}

impl DirectHttpKey {
    fn new(creds: DirectCredentials, device_id: String) -> Self {
        Self {
            base_url: creds.base_url,
            token: creds.token,
            device_id,
        }
    }
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
pub(crate) async fn login(
    sessions: &DirectSessions,
    base_url: String,
    token: String,
) -> Result<String, String> {
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

    let device_id = device_id()?;
    let creds = DirectCredentials {
        base_url: base.clone(),
        token,
    };
    let cached = DirectHttpCache::new(DirectHttpKey::new(creds.clone(), device_id))?;
    let resp = cached
        .http
        .client()
        .get(cached.http.url("/v1/status"))
        .timeout(REST_REPLY_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("could not reach Baybo: {e}"))?;

    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(INVALID_TOKEN_CODE.into());
    }
    if !resp.status().is_success() {
        return Err(format!("Baybo returned HTTP {}", resp.status().as_u16()));
    }

    let bytes = serde_json::to_vec(&creds).map_err(|e| e.to_string())?;
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

    sessions.set_http_client(cached);
    Ok(base)
}

/// The current direct connection's base URL, if credentials are persisted.
pub(crate) fn status() -> Result<Option<String>, String> {
    Ok(credentials()?.map(|c| c.base_url))
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

/// Read the stored direct credentials (base URL + gateway access token).
/// `Ok(None)` = not connected. The REST/WS/blob legs read both values from here.
pub(crate) fn credentials() -> Result<Option<DirectCredentials>, String> {
    let Some(bytes) = keychain::read_direct_credentials()? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| e.to_string())
}

pub(crate) fn device_id() -> Result<String, String> {
    let sign_key = keychain::load_or_create_device_sign_key()?;
    Ok(device_proto::delegation::device_id_for(
        &sign_key.verifying_key(),
    ))
}

/// Build the `/v1/channel-ws` URL from a stored base URL: `http→ws`, `https→wss`,
/// path `/v1/channel-ws`. The Bearer rides the upgrade request's
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
