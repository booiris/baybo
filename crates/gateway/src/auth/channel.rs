//! Channel auth middleware.
//!
//! The loopback channel listener accepts `x-baybo-channel-token` (or
//! `?token=` for runtimes that can't set custom headers on a WS
//! upgrade). Every entry in [`ChannelTokenTable`] carries a
//! [`ClientIdentity`] (pid + label); the middleware looks up the
//! presented token, distinguishes the bundled TUI from subprocess and
//! tool sidecars via reserved labels, and stashes the matching
//! [`AuthedClient`] on the request.
//!
//! The admin listener co-hosts `/v1/channel-ws` for browser web chat
//! tabs. That route is authenticated by the admin bearer middleware
//! before this module marks successful requests as [`AuthedClient::Web`].
//!
//! The TUI token is minted by the gateway at startup, written to the
//! secret vault under [`super::token::TUI_TOKEN_VAULT_KEY`], and
//! registered in the same [`ChannelTokenTable`] with
//! [`TUI_CLIENT_LABEL`]. The bundled `baybo tui` reads it back from
//! the vault and presents the same hex string. Subprocess sidecars
//! get their token via env var (see [`crate::spawn`]); each
//! [`crate::spawn::ChildHandle`] owns the
//! [`super::token::TokenHandle`] so the token is revoked the moment
//! the child drops.
//!
//! Transport isolation: the channel listener binds `127.0.0.1` only
//! (see [`crate::channel_listener`]). The vault file and the channel
//! port-file are both `0o600`, so a different UID can't read either —
//! the "same-UID attacker has already won" threat model is the boundary;
//! we don't need kernel-level peer-credential checks on top.

use std::sync::Arc;

use super::token::{
    CHANNEL_TOKEN_HEADER, ChannelTokenTable, ClientIdentity, TOOL_CLIENT_LABEL_PREFIX,
    TUI_CLIENT_LABEL,
};
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::Response;
use baybo_store::DeviceStore;

/// Tag placed on the request after auth succeeds so downstream
/// handlers know how the caller was authenticated.
#[derive(Debug, Clone)]
pub enum AuthedClient {
    Tui,
    /// Embedded tool-sidecar (browser MCP server today; future
    /// code_exec, db_query, …). Session-scoped like [`Self::Tui`],
    /// not per-bot/per-user, so it bypasses the per-channel pairing
    /// gate. Rejected from the channel-WS handshake — tool sidecars
    /// don't register as channels.
    Tool {
        /// Full label string starting with [`TOOL_CLIENT_LABEL_PREFIX`]
        /// (e.g. `"tool/browser"`). The suffix names the specific
        /// sidecar; future per-tool gating can match on it.
        label: String,
    },
    Subprocess {
        pid: u32,
        label: String,
        /// Channel type the token is bound to (mirrors
        /// [`ClientIdentity::bound_channel_type`]). `None` for legacy
        /// tokens minted before this field existed; handlers that need
        /// to gate on channel type (pairing, blob upload) must reject
        /// in that case.
        channel_type: Option<String>,
    },
    /// Admin-side web chat tab. Authenticated by the admin bearer on the
    /// admin listener, then constrained by the Register handshake to
    /// `ChannelType::HTTP`.
    Web,
    /// A paired, operator-approved iOS companion device. Resolved by its
    /// persisted `auth_token` against the [`DeviceStore`] (not the in-memory
    /// [`ChannelTokenTable`]) — only `approved` rows match, so a pending or
    /// revoked device never authenticates. Scoped to the channel surface:
    /// registers on `/v1/channel-ws` only as [`ChannelType::IOS`]
    /// (a `Subscribed` channel). HTTP `/v1/blobs` upload is forbidden for device
    /// tokens; mobile attachments upload over the E2E relay blob leg instead.
    Device {
        device_id: String,
    },
}

impl AuthedClient {
    fn from_identity(identity: ClientIdentity) -> Self {
        if identity.label == TUI_CLIENT_LABEL {
            AuthedClient::Tui
        } else if identity.label.starts_with(TOOL_CLIENT_LABEL_PREFIX) {
            AuthedClient::Tool {
                label: identity.label,
            }
        } else {
            AuthedClient::Subprocess {
                pid: identity.pid,
                label: identity.label,
                channel_type: identity.bound_channel_type,
            }
        }
    }
}

/// State shared with the channel auth middleware.
#[derive(Clone)]
pub struct ChannelAuthState {
    tokens: ChannelTokenTable,
    /// Persisted device registry, consulted when a presented token misses the
    /// in-memory table. `None` on listeners that never serve devices (the
    /// loopback sidecar/TUI listener), so they skip the extra lookup entirely.
    device_store: Option<Arc<dyn DeviceStore>>,
}

impl ChannelAuthState {
    pub fn new(tokens: ChannelTokenTable) -> Self {
        Self {
            tokens,
            device_store: None,
        }
    }

    /// Enable iOS-device auth: a token-table miss falls back to an
    /// approved-device lookup against `device_store`. Set only on the listener
    /// that serves `/v1/channel-ws` for paired devices.
    pub fn with_device_store(mut self, device_store: Arc<dyn DeviceStore>) -> Self {
        self.device_store = Some(device_store);
        self
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

/// Mark the already-admin-authenticated co-hosted channel routes as web chat.
pub fn attach_web_identity<S>(router: axum::Router<S>) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router.layer(middleware::from_fn(mark_web_identity))
}

async fn mark_web_identity(mut req: Request<Body>, next: Next) -> Response {
    req.extensions_mut().insert(AuthedClient::Web);
    next.run(req).await
}

/// Middleware: validates the channel token header (or `?token=` query),
/// stashes [`AuthedClient`] in request extensions, forwards.
///
/// All failure paths log at `debug!` under `baybo_gateway::auth::channel`
/// with enough context to diagnose "why 401?" without leaking the
/// secret. Enable with `RUST_LOG=baybo_gateway::auth::channel=debug`.
pub async fn require_channel_auth(
    State(state): State<ChannelAuthState>,
    mut req: Request<Body>,
    next: Next,
) -> std::result::Result<Response, StatusCode> {
    let path = req.uri().path().to_owned();
    let has_tok_hdr = req.headers().contains_key(CHANNEL_TOKEN_HEADER);
    let has_tok_query = has_query_token(req.uri().query());

    // Extract the token synchronously (borrows `req`), then resolve it
    // asynchronously with the *owned* token only — so no `!Sync`
    // `&Request<Body>` is held across the device-store await and the handler
    // future stays `Send`.
    let resolved = match extract_token(&req) {
        Ok(Some(token)) => resolve_token(&state, &token).await,
        Ok(None) => Ok(None),
        Err(status) => Err(status),
    };
    match resolved {
        Ok(Some(authed)) => {
            match &authed {
                AuthedClient::Tui => {
                    tracing::debug!(%path, "channel auth: accepted via TUI token");
                }
                AuthedClient::Device { device_id, .. } => {
                    tracing::debug!(
                        %path, device_id = %device_id,
                        "channel auth: accepted via approved device token",
                    );
                }
                AuthedClient::Tool { label } => {
                    tracing::debug!(
                        %path, label = %label,
                        "channel auth: accepted via tool-sidecar token",
                    );
                }
                AuthedClient::Subprocess { pid, label, .. } => {
                    tracing::debug!(
                        %path, pid, label = %label,
                        "channel auth: accepted via subprocess token",
                    );
                }
                AuthedClient::Web => {
                    tracing::debug!(%path, "channel auth: accepted via web chat identity");
                }
            }
            req.extensions_mut().insert(authed);
            // Strip `?token=` from the URI before TraceLayer logs it.
            if let Some(sanitised) = sanitise_uri(req.uri()) {
                *req.uri_mut() = sanitised;
            }
            Ok(next.run(req).await)
        }
        Ok(None) => {
            tracing::warn!(
                %path,
                has_tok_hdr,
                has_tok_query,
                live_tokens = state.tokens.len(),
                "channel auth: no credential; rejecting with 401 \
                 (no x-baybo-channel-token header and no ?token= query)",
            );
            Err(StatusCode::UNAUTHORIZED)
        }
        Err(status) => {
            tracing::warn!(
                %path, %status,
                has_tok_hdr,
                has_tok_query,
                live_tokens = state.tokens.len(),
                "channel auth: token presented but lookup failed; rejecting",
            );
            Err(status)
        }
    }
}

/// Synchronously pull the presented token out of the request as an owned
/// `String` (header first, then `?token=` query). `Ok(None)` = no credential;
/// `Err` = a malformed (non-utf8) header. Kept sync + owned so the caller can
/// drop the `req` borrow before any await (see [`require_channel_auth`]).
///
/// The query form lets sidecar runtimes whose WebSocket client can't set
/// custom HTTP headers (any WHATWG `WebSocket`) still authenticate — a
/// loopback-only listener's access logs stay same-UID-local.
fn extract_token(req: &Request<Body>) -> std::result::Result<Option<String>, StatusCode> {
    match req.headers().get(CHANNEL_TOKEN_HEADER) {
        Some(value) => match value.to_str() {
            Ok(s) => Ok(Some(s.to_owned())),
            Err(e) => {
                tracing::debug!(error = %e, "channel auth: token header is not utf-8");
                Err(StatusCode::UNAUTHORIZED)
            }
        },
        None => Ok(token_from_query(req.uri().query())),
    }
}

/// Resolve a presented token to an [`AuthedClient`]: the in-memory channel
/// token table first, then (on a miss) an approved-device lookup. Takes the
/// owned token + `&state` only — no `req` borrow — so the future stays `Send`.
async fn resolve_token(
    state: &ChannelAuthState,
    token: &str,
) -> std::result::Result<Option<AuthedClient>, StatusCode> {
    if let Some(identity) = state.tokens.lookup(token) {
        return Ok(Some(AuthedClient::from_identity(identity)));
    }
    // In-memory miss: a paired iOS device presents a persisted `auth_token`,
    // not an in-table token. Only `approved` rows resolve (the store filters
    // status), so pending/revoked devices fall through to the 401 below.
    if let Some(device_store) = &state.device_store {
        match device_store.lookup_approved_by_auth_token(token).await {
            Ok(Some(row)) => {
                return Ok(Some(AuthedClient::Device {
                    device_id: row.device_id,
                }));
            }
            Ok(None) => {}
            Err(e) => {
                // Fail closed on a store error — don't leak the reason or
                // distinguish it from a genuine miss to the caller.
                tracing::warn!(error = %e, "channel auth: device token lookup failed");
                return Err(StatusCode::UNAUTHORIZED);
            }
        }
    }
    tracing::debug!(
        token_prefix = %short_token(token),
        token_len = token.len(),
        live_tokens = state.tokens.len(),
        "channel auth: token is not in live token table or device registry \
         (revoked, stale gateway-restart token, or never registered)",
    );
    Err(StatusCode::UNAUTHORIZED)
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

/// Test-only convenience: the synchronous extract + async resolve the
/// middleware does inline, behind the old single-call shape so the unit tests
/// read straight through.
#[cfg(test)]
async fn check_channel_token(
    req: &Request<Body>,
    state: &ChannelAuthState,
) -> std::result::Result<Option<AuthedClient>, StatusCode> {
    match extract_token(req)? {
        Some(token) => resolve_token(state, &token).await,
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::super::token::ClientIdentity;
    use super::*;
    use axum::body::Body;
    use axum::http::Request;

    fn mk_state() -> (ChannelAuthState, ChannelTokenTable) {
        let table = ChannelTokenTable::new();
        (ChannelAuthState::new(table.clone()), table)
    }

    fn empty_req() -> Request<Body> {
        Request::builder().uri("/v1/x").body(Body::empty()).unwrap()
    }

    fn ident(pid: u32, label: &str) -> ClientIdentity {
        ClientIdentity {
            pid,
            label: label.into(),
            bound_channel_type: None,
        }
    }

    #[tokio::test]
    async fn tui_token_label_resolves_to_tui_authed_client() {
        let (state, tokens) = mk_state();
        let handle = tokens.mint(ident(0, TUI_CLIENT_LABEL));
        let mut req = empty_req();
        req.headers_mut()
            .insert(CHANNEL_TOKEN_HEADER, handle.token().parse().unwrap());
        let out = check_channel_token(&req, &state).await.unwrap();
        assert!(matches!(out, Some(AuthedClient::Tui)));
    }

    #[tokio::test]
    async fn subprocess_token_label_resolves_to_subprocess() {
        let (state, tokens) = mk_state();
        let handle = tokens.mint(ident(1234, "telegram"));
        let mut req = empty_req();
        req.headers_mut()
            .insert(CHANNEL_TOKEN_HEADER, handle.token().parse().unwrap());
        let out = check_channel_token(&req, &state).await.unwrap();
        assert!(matches!(
            out,
            Some(AuthedClient::Subprocess { pid: 1234, .. })
        ));
    }

    #[tokio::test]
    async fn unknown_token_rejected() {
        let (state, _tokens) = mk_state();
        let mut req = empty_req();
        req.headers_mut()
            .insert(CHANNEL_TOKEN_HEADER, "deadbeef".parse().unwrap());
        let err = check_channel_token(&req, &state).await.unwrap_err();
        assert_eq!(err, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn missing_credential_returns_none() {
        let (state, _tokens) = mk_state();
        let req = empty_req();
        assert!(check_channel_token(&req, &state).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn token_revoked_after_handle_drop() {
        let (state, tokens) = mk_state();
        let token_str = {
            let handle = tokens.mint(ident(1234, "telegram"));
            handle.token().to_string()
            // handle drops here -> token revoked
        };
        let mut req = empty_req();
        req.headers_mut()
            .insert(CHANNEL_TOKEN_HEADER, token_str.parse().unwrap());
        let err = check_channel_token(&req, &state).await.unwrap_err();
        assert_eq!(err, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn token_query_param_accepted() {
        let (state, tokens) = mk_state();
        let handle = tokens.mint(ident(42, "telegram"));
        let uri = format!("/v1/channel-ws?token={}", handle.token());
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let out = check_channel_token(&req, &state).await.unwrap();
        assert!(matches!(
            out,
            Some(AuthedClient::Subprocess { pid: 42, .. })
        ));
    }

    #[tokio::test]
    async fn header_beats_query_on_mismatch() {
        // Header is canonical; if both are present and the header is
        // wrong we reject regardless of a valid query token. Locks in
        // the precedence so a later change doesn't silently let a
        // leaked query override a fresh header.
        let (state, tokens) = mk_state();
        let handle = tokens.mint(ident(42, "telegram"));
        let uri = format!("/v1/channel-ws?token={}", handle.token());
        let mut req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        req.headers_mut()
            .insert(CHANNEL_TOKEN_HEADER, "deadbeef".parse().unwrap());
        let err = check_channel_token(&req, &state).await.unwrap_err();
        assert_eq!(err, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn approved_device_token_resolves_to_device() {
        use baybo_storage::test_support::MemoryDeviceStore;
        use std::sync::Arc;

        let store = Arc::new(MemoryDeviceStore::new());
        store
            .create(&baybo_store::DeviceRow {
                device_id: "d1".into(),
                device_pubkey: vec![0u8; 32],
                auth_token: "devtok".into(),
                status: baybo_store::DeviceStatus::Approved,
                rendezvous_id: Some("11111111-2222-4333-8444-555555555555".into()),
                created_at: 1,
                approved_at: Some(2),
                last_seen_at: None,
                relay_url: "wss://relay.test".into(),
                remote_api_key: "inst-test".into(),
            })
            .await
            .unwrap();
        let (state, _tokens) = mk_state();
        let state = state.with_device_store(store);

        // The device presents its persisted token via `?token=`.
        let req = Request::builder()
            .uri("/v1/channel-ws?token=devtok")
            .body(Body::empty())
            .unwrap();
        let out = check_channel_token(&req, &state).await.unwrap();
        assert!(matches!(
            out,
            Some(AuthedClient::Device { device_id }) if device_id == "d1"
        ));
    }

    #[tokio::test]
    async fn revoked_device_token_is_rejected() {
        use baybo_storage::test_support::MemoryDeviceStore;
        use std::sync::Arc;

        let store = Arc::new(MemoryDeviceStore::new());
        store
            .create(&baybo_store::DeviceRow {
                device_id: "d1".into(),
                device_pubkey: vec![0u8; 32],
                auth_token: "revtok".into(),
                status: baybo_store::DeviceStatus::Revoked,
                rendezvous_id: Some("11111111-2222-4333-8444-555555555555".into()),
                created_at: 1,
                approved_at: Some(1),
                last_seen_at: None,
                relay_url: "wss://relay.test".into(),
                remote_api_key: "inst-test".into(),
            })
            .await
            .unwrap();
        let (state, _tokens) = mk_state();
        let state = state.with_device_store(store);

        let mut req = empty_req();
        req.headers_mut()
            .insert(CHANNEL_TOKEN_HEADER, "revtok".parse().unwrap());
        // A revoked device token must 401.
        let err = check_channel_token(&req, &state).await.unwrap_err();
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
