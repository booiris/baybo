//! Admin TCP bearer-token auth.
//!
//! The admin listener hosts config / status / jobs / cron / memory /
//! traces / skills / tools / llm and a read-only channel list — no chat
//! content. A single bearer token stored in the secret vault under
//! `gateway.admin_token` guards every `/v1/*` route.

use std::sync::Arc;

use aura_security::SecretVault;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::Response;
use rand::Rng;

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
}

impl AdminAuthState {
    pub fn new(token: String) -> Self {
        Self {
            expected: Arc::new(token),
        }
    }
}

/// Axum middleware: extracts the token (Authorization header preferred,
/// `?token=` query fallback), constant-time compares against the
/// vault-stored value, and strips `token` from the URI before passing
/// the request on so `tower_http::trace::TraceLayer` does not log it.
pub async fn require_admin_token(
    State(state): State<AdminAuthState>,
    mut req: Request<Body>,
    next: Next,
) -> std::result::Result<Response, StatusCode> {
    let Some(presented) = extract_token(&req) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    if !aura_gateway_auth::constant_time_eq(state.expected.as_bytes(), presented.as_bytes()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if let Some(sanitised) = sanitise_uri(req.uri()) {
        *req.uri_mut() = sanitised;
    }
    Ok(next.run(req).await)
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
