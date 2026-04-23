//! Channel auth middleware.
//!
//! Two authentication modes, checked on every request to the channel
//! WebSocket endpoint:
//!
//! * **TUI PSK** (`x-aura-tui-secret` header): hex-encoded effective
//!   PSK — the per-install secret derived from
//!   [`aura_gateway_auth::effective_tui_psk`]. Used by the bundled
//!   `aura tui` client.
//! * **Subprocess capability token** (`x-aura-channel-token` header):
//!   an entry in [`ChannelTokenTable`] that the gateway minted when
//!   it spawned the sidecar. Each live token maps to a specific
//!   [`aura_gateway_auth::ClientIdentity`] (PID + label); when the
//!   owning [`crate::spawn::ChildHandle`] drops, the token is
//!   revoked.
//!
//! Transport isolation: the channel listener binds `127.0.0.1` only
//! (see [`crate::channel_listener`]). Both secrets are delivered over
//! channels only the owning UID can reach (child env vars for the
//! token, salt file under the workspace for the PSK), so the
//! "same-UID attacker already wins" threat model is the boundary; we
//! don't need kernel-level peer-credential checks on top.

use std::sync::Arc;

use aura_gateway_auth::{
    CHANNEL_TOKEN_HEADER, ChannelTokenTable, TUI_PSK_HEADER, constant_time_eq,
};
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::Response;

/// Tag placed on the request after auth succeeds so downstream
/// handlers know how the caller was authenticated.
#[derive(Debug, Clone)]
pub enum AuthedClient {
    Tui,
    Subprocess { pid: u32, label: String },
}

/// State shared with the channel auth middleware.
#[derive(Clone)]
pub struct ChannelAuthState {
    psk: Arc<[u8; 32]>,
    tokens: ChannelTokenTable,
}

impl ChannelAuthState {
    pub fn new(psk: [u8; 32], tokens: ChannelTokenTable) -> Self {
        Self {
            psk: Arc::new(psk),
            tokens,
        }
    }
}

/// Apply the channel auth middleware to a router. Kept as a function
/// (rather than an exported Layer alias) because
/// `middleware::from_fn_with_state` produces an un-nameable type.
pub fn attach<S>(router: axum::Router<S>, state: ChannelAuthState) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router.layer(middleware::from_fn_with_state(state, require_channel_auth))
}

/// Middleware: validates one of the two header-based auth modes,
/// stashes [`AuthedClient`] in request extensions, forwards.
///
/// All failure paths log at `debug!` under `aura_gateway::auth_channel`
/// with enough context to diagnose "why 401?" without leaking the
/// secret. Enable with `RUST_LOG=aura_gateway::auth_channel=debug`.
pub async fn require_channel_auth(
    State(state): State<ChannelAuthState>,
    mut req: Request<Body>,
    next: Next,
) -> std::result::Result<Response, StatusCode> {
    let path = req.uri().path().to_owned();
    let has_psk_hdr = req.headers().contains_key(TUI_PSK_HEADER);
    let has_tok_hdr = req.headers().contains_key(CHANNEL_TOKEN_HEADER);

    // TUI PSK takes precedence — cheaper, common case.
    match check_tui_psk(&req, &state) {
        Ok(Some(authed)) => {
            tracing::debug!(%path, "channel auth: accepted via TUI PSK");
            req.extensions_mut().insert(authed);
            return Ok(next.run(req).await);
        }
        Ok(None) => {}
        Err(status) => {
            tracing::warn!(
                %path, %status,
                "channel auth: TUI PSK header present but did not match; rejecting",
            );
            return Err(status);
        }
    }

    // Fall through to subprocess token (header or `?token=` query).
    match check_child_token(&req, &state) {
        Ok(Some(authed)) => {
            if let AuthedClient::Subprocess { pid, label } = &authed {
                tracing::debug!(
                    %path, pid, label = %label,
                    "channel auth: accepted via subprocess token",
                );
            }
            req.extensions_mut().insert(authed);
            // Strip `?token=` from the URI before TraceLayer logs it.
            if let Some(sanitised) = sanitise_uri(req.uri()) {
                *req.uri_mut() = sanitised;
            }
            return Ok(next.run(req).await);
        }
        Ok(None) => {}
        Err(status) => {
            tracing::warn!(
                %path, %status,
                has_psk_hdr,
                has_tok_hdr,
                has_tok_query = has_query_token(req.uri().query()),
                live_tokens = state.tokens.len(),
                "channel auth: subprocess token present but lookup failed; rejecting",
            );
            return Err(status);
        }
    }

    tracing::warn!(
        %path,
        has_psk_hdr,
        has_tok_hdr,
        has_tok_query = has_query_token(req.uri().query()),
        live_tokens = state.tokens.len(),
        "channel auth: no valid credential; rejecting with 401 \
         (no x-aura-tui-secret / x-aura-channel-token header and no \
         ?token= query — sidecar likely running a stale bundle)",
    );
    Err(StatusCode::UNAUTHORIZED)
}

fn check_tui_psk(
    req: &Request<Body>,
    state: &ChannelAuthState,
) -> std::result::Result<Option<AuthedClient>, StatusCode> {
    let Some(value) = req.headers().get(TUI_PSK_HEADER) else {
        return Ok(None);
    };
    let bytes = hex::decode(value.as_bytes()).map_err(|e| {
        tracing::debug!(
            error = %e,
            header_bytes = value.len(),
            "channel auth: TUI PSK header is not valid hex",
        );
        StatusCode::UNAUTHORIZED
    })?;
    if constant_time_eq(state.psk.as_slice(), &bytes) {
        Ok(Some(AuthedClient::Tui))
    } else {
        tracing::debug!(
            decoded_len = bytes.len(),
            expected_len = state.psk.len(),
            "channel auth: TUI PSK header did not match expected value",
        );
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn check_child_token(
    req: &Request<Body>,
    state: &ChannelAuthState,
) -> std::result::Result<Option<AuthedClient>, StatusCode> {
    // Header first, then `?token=` query. The query form lets sidecar
    // runtimes whose WebSocket client can't set custom HTTP headers
    // (bun's native WHATWG WebSocket, any browser-style client) still
    // authenticate — a loopback-only listener's access logs stay
    // same-UID-local, so the usual "don't put secrets in URLs"
    // warning doesn't bite here.
    let token_cow = match req.headers().get(CHANNEL_TOKEN_HEADER) {
        Some(value) => match value.to_str() {
            Ok(s) => Some(std::borrow::Cow::Borrowed(s)),
            Err(e) => {
                tracing::debug!(error = %e, "channel auth: token header is not utf-8");
                return Err(StatusCode::UNAUTHORIZED);
            }
        },
        None => token_from_query(req.uri().query()).map(std::borrow::Cow::Owned),
    };
    let Some(token) = token_cow else {
        return Ok(None);
    };
    match state.tokens.lookup(token.as_ref()) {
        Some(identity) => Ok(Some(AuthedClient::Subprocess {
            pid: identity.pid,
            label: identity.label,
        })),
        None => {
            tracing::debug!(
                token_prefix = %short_token(token.as_ref()),
                token_len = token.len(),
                live_tokens = state.tokens.len(),
                "channel auth: subprocess token is not in live token table \
                 (revoked, expired child, or never registered)",
            );
            Err(StatusCode::UNAUTHORIZED)
        }
    }
}

/// Extract `token` from a URL query string. Hand-rolled so we don't
/// pull `url` or `form_urlencoded` into the hot path for one field.
fn token_from_query(query: Option<&str>) -> Option<String> {
    let q = query?;
    for pair in q.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == "token" {
            return percent_decode(v);
        }
    }
    None
}

/// Minimal `%XX` → byte decode. Tokens are hex-encoded (URL-safe
/// already), so this is mostly defense: if any encoding slips in we
/// handle it rather than silently mismatching on the literal `%XX`.
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16)?;
            let lo = (bytes[i + 2] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// First 6 hex chars of a token for log correlation without leaking
/// the full secret.
fn short_token(token: &str) -> String {
    token.chars().take(6).collect()
}

fn has_query_token(query: Option<&str>) -> bool {
    query.is_some_and(|q| q.split('&').any(|p| p.starts_with("token=")))
}

/// Strip `token=<…>` from the URI's query string so downstream
/// loggers (`tower_http::trace::TraceLayer`) never see the secret.
/// Returns `None` when no rewrite is needed.
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

#[cfg(test)]
mod tests {
    use super::*;
    use aura_gateway_auth::ClientIdentity;
    use axum::body::Body;
    use axum::http::Request;

    fn mk_state() -> (ChannelAuthState, ChannelTokenTable, [u8; 32]) {
        let psk = [7u8; 32];
        let table = ChannelTokenTable::new();
        (ChannelAuthState::new(psk, table.clone()), table, psk)
    }

    fn empty_req() -> Request<Body> {
        Request::builder().uri("/v1/x").body(Body::empty()).unwrap()
    }

    #[test]
    fn tui_psk_accepts_matching_hex() {
        let (state, _t, psk) = mk_state();
        let mut req = empty_req();
        req.headers_mut()
            .insert(TUI_PSK_HEADER, hex::encode(psk).parse().unwrap());
        let out = check_tui_psk(&req, &state).unwrap();
        assert!(matches!(out, Some(AuthedClient::Tui)));
    }

    #[test]
    fn tui_psk_rejects_wrong_value() {
        let (state, _t, _psk) = mk_state();
        let mut req = empty_req();
        req.headers_mut()
            .insert(TUI_PSK_HEADER, hex::encode([0u8; 32]).parse().unwrap());
        let err = check_tui_psk(&req, &state).unwrap_err();
        assert_eq!(err, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn tui_psk_ignores_missing_header() {
        let (state, _t, _psk) = mk_state();
        let req = empty_req();
        assert!(check_tui_psk(&req, &state).unwrap().is_none());
    }

    #[test]
    fn child_token_accepts_registered_token() {
        let (state, tokens, _psk) = mk_state();
        let handle = tokens.mint(ClientIdentity {
            pid: 1234,
            label: "telegram".into(),
        });
        let mut req = empty_req();
        req.headers_mut()
            .insert(CHANNEL_TOKEN_HEADER, handle.token().parse().unwrap());
        let out = check_child_token(&req, &state).unwrap();
        assert!(matches!(
            out,
            Some(AuthedClient::Subprocess { pid: 1234, .. })
        ));
    }

    #[test]
    fn child_token_rejects_unknown_token() {
        let (state, _tokens, _psk) = mk_state();
        let mut req = empty_req();
        req.headers_mut()
            .insert(CHANNEL_TOKEN_HEADER, "deadbeef".parse().unwrap());
        let err = check_child_token(&req, &state).unwrap_err();
        assert_eq!(err, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn child_token_revoked_after_handle_drop() {
        let (state, tokens, _psk) = mk_state();
        let token_str = {
            let handle = tokens.mint(ClientIdentity {
                pid: 1234,
                label: "telegram".into(),
            });
            handle.token().to_string()
            // handle drops here -> token revoked
        };
        let mut req = empty_req();
        req.headers_mut()
            .insert(CHANNEL_TOKEN_HEADER, token_str.parse().unwrap());
        let err = check_child_token(&req, &state).unwrap_err();
        assert_eq!(err, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn child_token_accepts_query_param() {
        let (state, tokens, _psk) = mk_state();
        let handle = tokens.mint(ClientIdentity {
            pid: 42,
            label: "telegram".into(),
        });
        let uri = format!("/v1/channel-ws?token={}", handle.token());
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let out = check_child_token(&req, &state).unwrap();
        assert!(matches!(
            out,
            Some(AuthedClient::Subprocess { pid: 42, .. })
        ));
    }

    #[test]
    fn child_token_header_beats_query_on_mismatch() {
        // Header is canonical; if both are present and the header is
        // wrong we reject regardless of a valid query token. Locks in
        // the precedence so a later change doesn't silently let a
        // leaked query override a fresh header.
        let (state, tokens, _psk) = mk_state();
        let handle = tokens.mint(ClientIdentity {
            pid: 42,
            label: "telegram".into(),
        });
        let uri = format!("/v1/channel-ws?token={}", handle.token());
        let mut req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        req.headers_mut()
            .insert(CHANNEL_TOKEN_HEADER, "deadbeef".parse().unwrap());
        let err = check_child_token(&req, &state).unwrap_err();
        assert_eq!(err, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn sanitise_uri_strips_token_and_keeps_others() {
        let u: Uri = "/v1/channel-ws?token=abc&x=1".parse().unwrap();
        let s = sanitise_uri(&u).unwrap();
        assert_eq!(s.path_and_query().unwrap().as_str(), "/v1/channel-ws?x=1");
    }

    #[test]
    fn sanitise_uri_drops_query_when_only_token() {
        let u: Uri = "/v1/channel-ws?token=abc".parse().unwrap();
        let s = sanitise_uri(&u).unwrap();
        assert_eq!(s.path_and_query().unwrap().as_str(), "/v1/channel-ws");
    }
}
