//! The relay service: the blind pairing-rendezvous server (C).
//!
//! Two asymmetric routes ride the same [`RelayBroker`], keyed by the SPAKE2
//! pairing `code`:
//!
//! - `GET /pair/host/{code}` — the **gateway** side. Authenticated by the
//!   gateway's admission key ([`INSTANCE_KEY_HEADER`]); on success it parks a
//!   leg under `code` and waits for the app.
//! - `GET /pair/join/{code}` — the **app** side. No credential (the app only
//!   has the scanned code). It [`try_match`]es an already-hosted code and is
//!   refused if no admitted gateway is hosting it.
//!
//! That asymmetry is the broker's admission: only an admitted gateway can
//! occupy a code, so an unauthenticated peer can pair with a waiting host but
//! can neither squat codes nor flood the broker with parked legs. C stays blind
//! — it copies opaque SPAKE2 frames and never sees the code's secret.
//!
//! [`try_match`]: RelayBroker::try_match

use std::collections::HashSet;
use std::sync::Arc;

use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use crate::broker::RelayBroker;
use crate::error::RelayError;
use crate::ws::pump_ws;

/// Default listener when `RELAY_BIND_ADDR` is unset; typically behind a TLS
/// terminator on the operator's relay host.
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:8444";

/// Header the gateway presents to host a rendezvous (its admission key). The app
/// side carries no credential.
pub const INSTANCE_KEY_HEADER: &str = "x-instance-key";

/// Runtime config for the relay service.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// `host:port` to bind the listener on.
    pub bind_addr: String,
    /// Admitted gateway instance keys (the host-leg allow-list).
    pub instance_keys: Vec<String>,
}

impl RelayConfig {
    /// Load from the environment. Required: `RELAY_INSTANCE_KEYS`
    /// (comma-separated). Optional: `RELAY_BIND_ADDR`.
    pub fn from_env() -> Result<Self, RelayError> {
        let instance_keys: Vec<String> = std::env::var("RELAY_INSTANCE_KEYS")
            .map_err(|_| RelayError::Config("missing env RELAY_INSTANCE_KEYS".into()))?
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        if instance_keys.is_empty() {
            return Err(RelayError::Config(
                "RELAY_INSTANCE_KEYS must list at least one admitted gateway key".into(),
            ));
        }
        Ok(Self {
            bind_addr: std::env::var("RELAY_BIND_ADDR")
                .unwrap_or_else(|_| DEFAULT_BIND_ADDR.into()),
            instance_keys,
        })
    }
}

#[derive(Clone)]
struct RelayState {
    broker: Arc<RelayBroker>,
    admitted: Arc<HashSet<String>>,
}

/// Assemble the relay router from config.
pub fn build_router(config: &RelayConfig) -> Router {
    let state = RelayState {
        broker: Arc::new(RelayBroker::new()),
        admitted: Arc::new(config.instance_keys.iter().cloned().collect()),
    };
    Router::new()
        .route("/pair/host/{code}", get(host_handler))
        .route("/pair/join/{code}", get(join_handler))
        .with_state(state)
}

/// Gateway side: authenticate the instance key, then park a leg under `code`.
async fn host_handler(
    Path(code): Path<String>,
    State(state): State<RelayState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let admitted = headers
        .get(INSTANCE_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|k| state.admitted.contains(k));
    if !admitted {
        return (StatusCode::UNAUTHORIZED, "instance key not admitted").into_response();
    }
    let leg = state.broker.join(&code);
    let broker = Arc::clone(&state.broker);
    ws.on_upgrade(move |socket| async move {
        pump_ws(socket, leg).await;
        // If the app never matched (the host disconnected first), drop the
        // still-parked leg so a stale code can't linger.
        broker.cancel(&code);
    })
}

/// App side: match an already-hosted code; never park.
async fn join_handler(
    Path(code): Path<String>,
    State(state): State<RelayState>,
    ws: WebSocketUpgrade,
) -> Response {
    match state.broker.try_match(&code) {
        Some(leg) => ws.on_upgrade(move |socket| pump_ws(socket, leg)),
        None => (StatusCode::NOT_FOUND, "no pairing host for this code").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use tokio::net::TcpStream;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::StatusCode as WsStatus;
    use tokio_tungstenite::tungstenite::{Error as WsError, Message};
    use tokio_tungstenite::{WebSocketStream, client_async};

    async fn serve() -> u16 {
        let config = RelayConfig {
            bind_addr: "127.0.0.1:0".into(),
            instance_keys: vec!["inst-A".into()],
        };
        let listener = tokio::net::TcpListener::bind(&config.bind_addr)
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = build_router(&config);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        port
    }

    async fn connect_host(port: u16, code: &str, key: Option<&str>) -> WebSocketStream<TcpStream> {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut req = format!("ws://127.0.0.1:{port}/pair/host/{code}")
            .into_client_request()
            .unwrap();
        if let Some(k) = key {
            req.headers_mut()
                .insert(INSTANCE_KEY_HEADER, k.parse().unwrap());
        }
        client_async(req, stream).await.unwrap().0
    }

    async fn connect_join(port: u16, code: &str) -> Result<WebSocketStream<TcpStream>, WsError> {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let req = format!("ws://127.0.0.1:{port}/pair/join/{code}")
            .into_client_request()
            .unwrap();
        Ok(client_async(req, stream).await?.0)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn admitted_host_and_app_rendezvous_blind() {
        let port = serve().await;
        // The host parks synchronously inside the handler before its 101, so by
        // the time `connect_host` returns the leg is already waiting to match.
        let mut host = connect_host(port, "CODE1", Some("inst-A")).await;
        let mut app = connect_join(port, "CODE1").await.expect("host is parked");

        host.send(Message::Binary(b"a->p".to_vec()))
            .await
            .unwrap();
        assert_eq!(recv_bin(&mut app).await, b"a->p");
        app.send(Message::Binary(b"p->a".to_vec()))
            .await
            .unwrap();
        assert_eq!(recv_bin(&mut host).await, b"p->a");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unadmitted_host_is_rejected() {
        let port = serve().await;
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let req = format!("ws://127.0.0.1:{port}/pair/host/CODE2")
            .into_client_request()
            .unwrap();
        // No instance-key header → the upgrade is refused with 401.
        match client_async(req, stream).await {
            Err(WsError::Http(resp)) => assert_eq!(resp.status(), WsStatus::UNAUTHORIZED),
            other => panic!("expected 401, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn join_without_host_is_refused() {
        let port = serve().await;
        match connect_join(port, "NOHOST").await {
            Err(WsError::Http(resp)) => assert_eq!(resp.status(), WsStatus::NOT_FOUND),
            other => panic!("expected 404, got {other:?}"),
        }
    }

    async fn recv_bin(ws: &mut WebSocketStream<TcpStream>) -> Vec<u8> {
        loop {
            match ws.next().await.unwrap().unwrap() {
                Message::Binary(b) => return b.to_vec(),
                Message::Ping(_) | Message::Pong(_) => continue,
                other => panic!("unexpected frame: {other:?}"),
            }
        }
    }
}
