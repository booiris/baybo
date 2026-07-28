//! Admin TCP bearer-token auth.
//!
//! The admin listener hosts config / status / jobs / cron / memory /
//! traces / skills / tools / llm, chat REST, and the co-hosted channel
//! WS/blob routes. The admin bearer stored in the secret vault under
//! `gateway.admin_token` guards ordinary Web requests. When
//! `x-baybo-device-id` is present, the presented bearer must either be that
//! admin token or a bearer whose digest matches an approved device row; only
//! then is the request tagged as `AuthedClient::Device`.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::Response;
use baybo_security::SecretVault;
use baybo_store::DeviceStore;
use rand::Rng;

use super::channel::{AuthedClient, DEVICE_ID_HEADER};
use crate::{GatewayError, Result};

const TOKEN_SECRET_NAME: &str = "gateway.admin_token";
const TOKEN_BYTE_LEN: usize = 32;

/// Token lifecycle manager backed by [`SecretVault`].
pub struct AdminToken {
    vault: Arc<SecretVault>,
}

impl AdminToken {
    pub fn new(vault: Arc<SecretVault>) -> Self {
        Self { vault }
    }

    /// Read the current token, or `None` if none is stored.
    pub async fn get(&self) -> Result<Option<String>> {
        match self
            .vault
            .get_secret(TOKEN_SECRET_NAME)
            .await
            .map_err(|e| GatewayError::Vault(e.to_string()))?
        {
            Some(v) => String::from_utf8(v.as_bytes().to_vec())
                .map(Some)
                .map_err(|e| GatewayError::Vault(format!("token bytes not utf8: {e}"))),
            None => Ok(None),
        }
    }

    /// Mint a new 256-bit token if one is not already present. Returns
    /// the current token either way.
    pub async fn mint_if_absent(&self) -> Result<String> {
        if let Some(existing) = self.get().await? {
            return Ok(existing);
        }
        let token = generate_token();
        self.vault
            .store_secret(TOKEN_SECRET_NAME, token.as_bytes())
            .await
            .map_err(|e| GatewayError::Vault(e.to_string()))?;
        Ok(token)
    }

    /// Rotate the token unconditionally, returning the new value.
    pub async fn rotate(&self) -> Result<String> {
        let token = generate_token();
        self.vault
            .store_secret(TOKEN_SECRET_NAME, token.as_bytes())
            .await
            .map_err(|e| GatewayError::Vault(e.to_string()))?;
        Ok(token)
    }
}

fn generate_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTE_LEN];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// State shared with the admin auth middleware.
#[derive(Clone)]
pub struct AdminAuthState {
    expected: Arc<String>,
    device_store: Option<Arc<dyn DeviceStore>>,
}

impl AdminAuthState {
    pub fn new(token: String) -> Self {
        Self {
            expected: Arc::new(token),
            device_store: None,
        }
    }

    pub fn with_device_store(mut self, device_store: Arc<dyn DeviceStore>) -> Self {
        self.device_store = Some(device_store);
        self
    }

    /// The gateway's own admin bearer, for in-process forwards that re-enter
    /// [`require_admin_token`].
    ///
    /// Used by the relay API tunnel, whose caller is already authenticated by
    /// Noise IK static-key match before any HTTP is synthesised. It cannot
    /// present the device's own bearer (storage keeps only a digest) and should
    /// not: paired with `x-baybo-device-id` this lands on the branch of
    /// [`Self::authenticate_request`] that exists for exactly this case, so the
    /// caller keeps its [`AuthedClient::Device`] identity and the tunnel keeps
    /// the middleware instead of bypassing it.
    pub(crate) fn admin_bearer(&self) -> &str {
        &self.expected
    }

    /// Validate the bearer on `req`, resolve the downstream caller identity, and
    /// strip a `?token=...` query parameter before downstream layers can log the
    /// URI.
    pub async fn authenticate_request(
        &self,
        req: &mut Request<Body>,
    ) -> std::result::Result<AuthedClient, StatusCode> {
        let Some(presented) = extract_token(req) else {
            tracing::warn!(
                path = %req.uri().path(),
                has_auth_hdr = req.headers().contains_key(axum::http::header::AUTHORIZATION),
                has_device_hdr = req.headers().contains_key(DEVICE_ID_HEADER),
                "admin auth: no Bearer header and no ?token= query; rejecting with 401",
            );
            return Err(StatusCode::UNAUTHORIZED);
        };
        let device_id = device_id_claim(req.headers())?;
        let is_admin_token =
            super::token::constant_time_eq(self.expected.as_bytes(), presented.as_bytes());

        let authed = match device_id {
            Some(device_id) if is_admin_token => AuthedClient::Device { device_id },
            Some(device_id) => {
                self.authenticate_device_token(&presented, device_id)
                    .await?
            }
            None if is_admin_token => AuthedClient::Web,
            None => {
                tracing::warn!(
                    path = %req.uri().path(),
                    "admin auth: presented credential is not the admin token and no \
                     device header accompanies it; rejecting with 401",
                );
                return Err(StatusCode::UNAUTHORIZED);
            }
        };

        if let Some(sanitised) = sanitise_uri(req.uri()) {
            *req.uri_mut() = sanitised;
        }
        Ok(authed)
    }

    async fn authenticate_device_token(
        &self,
        presented: &str,
        claimed_device_id: String,
    ) -> std::result::Result<AuthedClient, StatusCode> {
        let Some(device_store) = &self.device_store else {
            tracing::warn!(
                claimed_device_id = %claimed_device_id,
                "admin auth: device header presented but this listener has no device \
                 store wired; rejecting with 401",
            );
            return Err(StatusCode::UNAUTHORIZED);
        };
        match device_store.lookup_approved_by_auth_token(presented).await {
            Ok(Some(row)) if row.device_id == claimed_device_id => Ok(AuthedClient::Device {
                device_id: claimed_device_id,
            }),
            Ok(Some(row)) => {
                tracing::warn!(
                    claimed_device_id = %claimed_device_id,
                    token_device_id = %row.device_id,
                    "admin auth: device header does not match bearer token"
                );
                Err(StatusCode::UNAUTHORIZED)
            }
            Ok(None) => {
                tracing::warn!(
                    claimed_device_id = %claimed_device_id,
                    "admin auth: bearer matches no approved device row; rejecting with 401",
                );
                Err(StatusCode::UNAUTHORIZED)
            }
            Err(e) => {
                tracing::warn!(error = %e, "admin auth: device token lookup failed");
                Err(StatusCode::UNAUTHORIZED)
            }
        }
    }
}

/// Axum middleware: extracts the token (Authorization header preferred,
/// `?token=` query fallback), resolves Web vs. Device identity, and strips
/// `token` from the URI before passing the request on so
/// `tower_http::trace::TraceLayer` does not log it.
pub async fn require_admin_token(
    State(state): State<AdminAuthState>,
    mut req: Request<Body>,
    next: Next,
) -> std::result::Result<Response, StatusCode> {
    let authed = state.authenticate_request(&mut req).await?;
    req.extensions_mut().insert(authed);
    Ok(next.run(req).await)
}

fn device_id_claim(headers: &HeaderMap) -> std::result::Result<Option<String>, StatusCode> {
    let Some(value) = headers.get(DEVICE_ID_HEADER) else {
        return Ok(None);
    };
    let device_id = value.to_str().map_err(|_| StatusCode::BAD_REQUEST)?.trim();
    if device_id.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if device_proto::delegation::device_pubkey_from_id(device_id).is_err() {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(Some(device_id.to_owned()))
}

fn extract_token(req: &Request<Body>) -> Option<String> {
    if let Some(value) = req.headers().get(axum::http::header::AUTHORIZATION)
        && let Ok(s) = value.to_str()
        && let Some(rest) = s.strip_prefix("Bearer ")
    {
        return Some(rest.trim().to_owned());
    }
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some(rest) = pair.strip_prefix("token=") {
                return Some(urlencoding_decode(rest));
            }
        }
    }
    None
}

fn sanitise_uri(uri: &Uri) -> Option<Uri> {
    let query = uri.query()?;
    let filtered: Vec<&str> = query
        .split('&')
        .filter(|p| !p.starts_with("token="))
        .collect();
    let new_pq = if filtered.is_empty() {
        uri.path().to_owned()
    } else {
        format!("{}?{}", uri.path(), filtered.join("&"))
    };
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(new_pq.parse().ok()?);
    Uri::from_parts(parts).ok()
}

fn urlencoding_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = from_hex(bytes[i + 1]);
                let lo = from_hex(bytes[i + 2]);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h << 4) | l);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_owned())
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitise_uri_strips_token() {
        let u: Uri = "/v1/status?token=abc&x=1".parse().unwrap();
        let s = sanitise_uri(&u).unwrap();
        assert_eq!(s.path_and_query().unwrap().as_str(), "/v1/status?x=1");
    }

    #[test]
    fn sanitise_uri_strips_only_token() {
        let u: Uri = "/v1/status?token=abc".parse().unwrap();
        let s = sanitise_uri(&u).unwrap();
        assert_eq!(s.path_and_query().unwrap().as_str(), "/v1/status");
    }

    #[test]
    fn generate_token_is_hex_64() {
        let t = generate_token();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
