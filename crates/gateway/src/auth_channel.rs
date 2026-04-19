//! Channel UDS auth middleware.
//!
//! Two complementary checks — either alone is insufficient:
//!
//! 1. **Peer credential** (SO_PEERCRED / getpeereid) — reject if the
//!    connecting process is not the same UID as the gateway. The UDS
//!    listener injects [`PeerCred`] as a request extension on every
//!    accepted connection.
//! 2. **Capability header**:
//!    * TUI: `x-aura-tui-secret` matching the per-install effective PSK;
//!    * Subprocess plugin: `x-aura-channel-token` matching a live entry
//!      in the [`ChannelTokenTable`] whose recorded PID equals the
//!      connecting peer's PID.

use std::sync::Arc;

use aura_gateway_auth::{
    CHANNEL_TOKEN_HEADER, ChannelTokenTable, TUI_PSK_HEADER, constant_time_eq,
};
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;

/// Peer credentials extracted from the accepted `UnixStream` and
/// stashed into each request as an axum extension.
#[derive(Debug, Clone, Copy)]
pub struct PeerCred {
    pub uid: u32,
    pub pid: Option<u32>,
}

/// Tag placed on the request after auth succeeds so downstream handlers
/// know how the caller was authenticated.
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
    expected_uid: u32,
}

impl ChannelAuthState {
    pub fn new(psk: [u8; 32], tokens: ChannelTokenTable, expected_uid: u32) -> Self {
        Self {
            psk: Arc::new(psk),
            tokens,
            expected_uid,
        }
    }
}

/// Apply the channel auth middleware to a router. Kept as a function
/// (rather than an exported Layer alias) because `middleware::from_fn_with_state`
/// produces an un-nameable type.
pub fn attach<S>(router: axum::Router<S>, state: ChannelAuthState) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router.layer(middleware::from_fn_with_state(state, require_channel_auth))
}

/// Middleware: validates peer UID + header, injects [`AuthedClient`]
/// into the request extensions, and forwards.
pub async fn require_channel_auth(
    State(state): State<ChannelAuthState>,
    mut req: Request<Body>,
    next: Next,
) -> std::result::Result<Response, StatusCode> {
    // Peer UID must match the gateway's UID. If the listener couldn't
    // produce a credential at all, reject closed — unknown peer is
    // never OK on a channel listener.
    let Some(peer) = req.extensions().get::<PeerCred>().copied() else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    if peer.uid != state.expected_uid {
        return Err(StatusCode::UNAUTHORIZED);
    }

    // TUI PSK takes precedence — cheaper, common case.
    if let Some(authed) = check_tui_psk(&req, &state)? {
        req.extensions_mut().insert(authed);
        return Ok(next.run(req).await);
    }

    // Fall through to subprocess token.
    if let Some(authed) = check_child_token(&req, &state, peer)? {
        req.extensions_mut().insert(authed);
        return Ok(next.run(req).await);
    }

    Err(StatusCode::UNAUTHORIZED)
}

fn check_tui_psk(
    req: &Request<Body>,
    state: &ChannelAuthState,
) -> std::result::Result<Option<AuthedClient>, StatusCode> {
    let Some(value) = req.headers().get(TUI_PSK_HEADER) else {
        return Ok(None);
    };
    let bytes = hex::decode(value.as_bytes()).map_err(|_| StatusCode::UNAUTHORIZED)?;
    if constant_time_eq(state.psk.as_slice(), &bytes) {
        Ok(Some(AuthedClient::Tui))
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn check_child_token(
    req: &Request<Body>,
    state: &ChannelAuthState,
    peer: PeerCred,
) -> std::result::Result<Option<AuthedClient>, StatusCode> {
    let Some(value) = req.headers().get(CHANNEL_TOKEN_HEADER) else {
        return Ok(None);
    };
    let token = value.to_str().map_err(|_| StatusCode::UNAUTHORIZED)?;
    let identity = state.tokens.lookup(token).ok_or(StatusCode::UNAUTHORIZED)?;
    // PID must match the recorded child so a stolen token can't be
    // replayed from a different process in the same UID.
    match peer.pid {
        Some(pid) if pid == identity.pid => Ok(Some(AuthedClient::Subprocess {
            pid: identity.pid,
            label: identity.label,
        })),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_gateway_auth::ClientIdentity;
    use axum::body::Body;
    use axum::http::Request;

    fn mk_state(uid: u32) -> (ChannelAuthState, ChannelTokenTable, [u8; 32]) {
        let psk = [7u8; 32];
        let table = ChannelTokenTable::new();
        (ChannelAuthState::new(psk, table.clone(), uid), table, psk)
    }

    fn empty_req() -> Request<Body> {
        Request::builder().uri("/v1/x").body(Body::empty()).unwrap()
    }

    #[test]
    fn tui_psk_accepts_matching_hex() {
        let (state, _t, psk) = mk_state(1000);
        let mut req = empty_req();
        req.headers_mut()
            .insert(TUI_PSK_HEADER, hex::encode(psk).parse().unwrap());
        let out = check_tui_psk(&req, &state).unwrap();
        assert!(matches!(out, Some(AuthedClient::Tui)));
    }

    #[test]
    fn tui_psk_rejects_wrong_value() {
        let (state, _t, _psk) = mk_state(1000);
        let mut req = empty_req();
        req.headers_mut()
            .insert(TUI_PSK_HEADER, hex::encode([0u8; 32]).parse().unwrap());
        let err = check_tui_psk(&req, &state).unwrap_err();
        assert_eq!(err, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn tui_psk_ignores_missing_header() {
        let (state, _t, _psk) = mk_state(1000);
        let req = empty_req();
        assert!(check_tui_psk(&req, &state).unwrap().is_none());
    }

    #[test]
    fn child_token_accepts_matching_pid() {
        let (state, tokens, _psk) = mk_state(1000);
        let handle = tokens.mint(ClientIdentity {
            pid: 1234,
            label: "telegram".into(),
        });
        let mut req = empty_req();
        req.headers_mut()
            .insert(CHANNEL_TOKEN_HEADER, handle.token().parse().unwrap());
        let peer = PeerCred {
            uid: 1000,
            pid: Some(1234),
        };
        let out = check_child_token(&req, &state, peer).unwrap();
        assert!(matches!(
            out,
            Some(AuthedClient::Subprocess { pid: 1234, .. })
        ));
    }

    #[test]
    fn child_token_rejects_wrong_pid() {
        let (state, tokens, _psk) = mk_state(1000);
        let handle = tokens.mint(ClientIdentity {
            pid: 1234,
            label: "telegram".into(),
        });
        let mut req = empty_req();
        req.headers_mut()
            .insert(CHANNEL_TOKEN_HEADER, handle.token().parse().unwrap());
        let peer = PeerCred {
            uid: 1000,
            pid: Some(999),
        };
        let err = check_child_token(&req, &state, peer).unwrap_err();
        assert_eq!(err, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn child_token_rejects_unknown_token() {
        let (state, _tokens, _psk) = mk_state(1000);
        let mut req = empty_req();
        req.headers_mut()
            .insert(CHANNEL_TOKEN_HEADER, "deadbeef".parse().unwrap());
        let peer = PeerCred {
            uid: 1000,
            pid: Some(1234),
        };
        let err = check_child_token(&req, &state, peer).unwrap_err();
        assert_eq!(err, StatusCode::UNAUTHORIZED);
    }
}
