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

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use remote_host_admission::Admission;

use crate::broker::RelayBroker;
use crate::control::{ControlHello, ControlRegistry};
use crate::ws::pump_ws;

/// Header the gateway presents to host a rendezvous (its admission key). The app
/// side carries no credential.
pub const INSTANCE_KEY_HEADER: &str = "x-instance-key";

/// Drop a gateway control connection that has gone silent for this long. The
/// gateway keepalive-Pings well inside it (every 30s), so only a half-open
/// connection trips it — releasing the stale registry slot promptly.
const CONTROL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Clone)]
struct RelayState {
    broker: Arc<RelayBroker>,
    admitted: Arc<dyn Admission>,
    /// Live gateway control connections, keyed by `relay_node_id`.
    control: Arc<ControlRegistry>,
    /// Monotonic source of per-data-leg `relay_key`s. Uniqueness (not secrecy)
    /// is all that's needed: the content-host route is admission-gated, so a
    /// guessed key can't be hosted by anyone but the real gateway.
    seq: Arc<AtomicU64>,
}

/// Assemble the relay router. `admission` is the shared, hot-reloaded allow-list
/// of admitted gateway instance keys.
pub fn build_router(admission: Arc<dyn Admission>) -> Router {
    let state = RelayState {
        broker: Arc::new(RelayBroker::new()),
        admitted: admission,
        control: Arc::new(ControlRegistry::new()),
        seq: Arc::new(AtomicU64::new(0)),
    };
    Router::new()
        .route("/pair/host/{code}", get(host_handler))
        .route("/pair/join/{code}", get(join_handler))
        // Content relay (phase 2): the gateway holds a control connection; a
        // phone names the gateway by relay_node_id and C splices a data leg.
        .route("/control", get(control_handler))
        .route("/content/join/{relay_node_id}", get(content_join_handler))
        .route("/content/host/{relay_key}", get(content_host_handler))
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
        .is_some_and(|k| state.admitted.is_admitted(k));
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

/// Gateway control connection. The gateway dials this and holds it open; the
/// first frame is a JSON [`ControlHello`] (admission key + `relay_node_id`).
/// After admission, C pushes `ControlSignal`s ("a phone arrived, open a data
/// leg") over it. Admission rides the hello (not a header) so the same dial both
/// authenticates and identifies the gateway.
async fn control_handler(State(state): State<RelayState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| run_control(socket, state))
}

async fn run_control(mut socket: WebSocket, state: RelayState) {
    let hello = match socket.recv().await {
        Some(Ok(AxumMessage::Binary(b))) => match serde_json::from_slice::<ControlHello>(&b) {
            Ok(h) => h,
            Err(e) => {
                tracing::debug!(error = %e, "control: unparseable hello; closing");
                return;
            }
        },
        _ => return,
    };
    if !state.admitted.is_admitted(&hello.instance_key) {
        tracing::warn!("control: instance key not admitted; closing");
        return;
    }
    tracing::info!(node = %hello.relay_node_id, "control: gateway connected");
    let mut rx = state.control.register(&hello.relay_node_id);
    // `register` replaces any prior slot (reconnect wins); if our slot is
    // superseded, `rx` closes and we must NOT unregister the new owner.
    let mut superseded = false;
    loop {
        tokio::select! {
            sig = rx.recv() => match sig {
                Some(signal) => {
                    let json = match serde_json::to_vec(&signal) {
                        Ok(j) => j,
                        Err(_) => continue,
                    };
                    if socket.send(AxumMessage::Binary(json.into())).await.is_err() {
                        break;
                    }
                }
                None => {
                    superseded = true;
                    break;
                }
            },
            inbound = socket.recv() => match inbound {
                // The gateway speaks only the hello; anything else (its keepalive
                // Pings, auto-Ponged by axum) just proves liveness.
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            // The gateway pings well within this window; silence past it means a
            // half-open connection, so drop it (and unregister) rather than pin a
            // dead slot until the OS TCP timeout.
            _ = tokio::time::sleep(CONTROL_IDLE_TIMEOUT) => {
                tracing::debug!(node = %hello.relay_node_id, "control: idle timeout; closing");
                break;
            }
        }
    }
    if !superseded {
        state.control.unregister(&hello.relay_node_id);
    }
    tracing::info!(node = %hello.relay_node_id, "control: gateway disconnected");
}

/// App side of a content session: the phone names the gateway by `relay_node_id`.
/// C signals that gateway (over its control connection) to open a data leg under
/// a fresh `relay_key`, then joins the phone's leg to it — blind. Refused fast
/// if no gateway is currently connected for that id.
async fn content_join_handler(
    Path(relay_node_id): Path<String>,
    State(state): State<RelayState>,
    ws: WebSocketUpgrade,
) -> Response {
    let relay_key = format!("dl-{}", state.seq.fetch_add(1, Ordering::Relaxed));
    if !state.control.signal_open(&relay_node_id, &relay_key).await {
        return (StatusCode::NOT_FOUND, "gateway not connected").into_response();
    }
    let leg = state.broker.join(&relay_key);
    let broker = Arc::clone(&state.broker);
    ws.on_upgrade(move |socket| async move {
        pump_ws(socket, leg).await;
        broker.cancel(&relay_key);
    })
}

/// Gateway side of a content session: the gateway, signaled over its control
/// connection, opens this leg under the C-issued `relay_key` and is matched to
/// the waiting phone. Admission-gated like the pairing host so only the real
/// gateway can occupy the leg.
async fn content_host_handler(
    Path(relay_key): Path<String>,
    State(state): State<RelayState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let admitted = headers
        .get(INSTANCE_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|k| state.admitted.is_admitted(k));
    if !admitted {
        return (StatusCode::UNAUTHORIZED, "instance key not admitted").into_response();
    }
    let leg = state.broker.join(&relay_key);
    let broker = Arc::clone(&state.broker);
    ws.on_upgrade(move |socket| async move {
        pump_ws(socket, leg).await;
        broker.cancel(&relay_key);
    })
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
        let admission = Arc::new(remote_host_admission::InMemoryAdmission::with_keys(["inst-A"]));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = build_router(admission);
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

        host.send(Message::Binary(b"a->p".to_vec())).await.unwrap();
        assert_eq!(recv_bin(&mut app).await, b"a->p");
        app.send(Message::Binary(b"p->a".to_vec())).await.unwrap();
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

    async fn connect_control(port: u16) -> WebSocketStream<TcpStream> {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let req = format!("ws://127.0.0.1:{port}/control")
            .into_client_request()
            .unwrap();
        client_async(req, stream).await.unwrap().0
    }

    async fn connect_content_join(
        port: u16,
        node: &str,
    ) -> Result<WebSocketStream<TcpStream>, WsError> {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let req = format!("ws://127.0.0.1:{port}/content/join/{node}")
            .into_client_request()
            .unwrap();
        Ok(client_async(req, stream).await?.0)
    }

    async fn connect_content_host(
        port: u16,
        key: &str,
        instance_key: &str,
    ) -> WebSocketStream<TcpStream> {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut req = format!("ws://127.0.0.1:{port}/content/host/{key}")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert(INSTANCE_KEY_HEADER, instance_key.parse().unwrap());
        client_async(req, stream).await.unwrap().0
    }

    async fn recv_control_json(ws: &mut WebSocketStream<TcpStream>) -> serde_json::Value {
        loop {
            match ws.next().await.unwrap().unwrap() {
                Message::Binary(b) => return serde_json::from_slice(&b).unwrap(),
                Message::Ping(_) | Message::Pong(_) => continue,
                other => panic!("unexpected control frame: {other:?}"),
            }
        }
    }

    /// Full content path: an admitted gateway holds a control connection; a
    /// phone names it by relay_node_id; C signals the gateway, which opens a data
    /// leg; the two legs splice and opaque bytes flow blind both ways.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn content_relay_splices_phone_and_gateway() {
        let port = serve().await;

        // Gateway holds its control connection (admitted) under node-1.
        let mut control = connect_control(port).await;
        let hello = serde_json::json!({ "relay_node_id": "node-1", "instance_key": "inst-A" });
        control
            .send(Message::Binary(serde_json::to_vec(&hello).unwrap()))
            .await
            .unwrap();

        // The phone dials content/join. Control registration is async, so retry
        // briefly until C admits the join (the gateway is registered).
        let mut app = None;
        for _ in 0..40 {
            match connect_content_join(port, "node-1").await {
                Ok(ws) => {
                    app = Some(ws);
                    break;
                }
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
            }
        }
        let mut app = app.expect("content/join admitted once the gateway control registers");

        // C signaled the gateway to open a data leg; read the relay_key.
        let signal = recv_control_json(&mut control).await;
        assert_eq!(signal["t"], "open_data_leg");
        let relay_key = signal["relay_key"].as_str().unwrap().to_owned();

        // The gateway opens the data leg; the legs splice, bytes flow blind.
        let mut gw = connect_content_host(port, &relay_key, "inst-A").await;
        app.send(Message::Binary(b"noise-up".to_vec()))
            .await
            .unwrap();
        assert_eq!(recv_bin(&mut gw).await, b"noise-up");
        gw.send(Message::Binary(b"noise-down".to_vec()))
            .await
            .unwrap();
        assert_eq!(recv_bin(&mut app).await, b"noise-down");
    }

    /// A phone whose gateway has no live control connection is refused fast.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn content_join_without_a_connected_gateway_is_refused() {
        let port = serve().await;
        match connect_content_join(port, "ghost-node").await {
            Err(WsError::Http(resp)) => assert_eq!(resp.status(), WsStatus::NOT_FOUND),
            other => panic!("expected 404, got {other:?}"),
        }
    }

    /// An unadmitted gateway control hello is dropped (the WS closes), so it can
    /// never receive signals or be named by a phone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unadmitted_control_hello_is_dropped() {
        let port = serve().await;
        let mut control = connect_control(port).await;
        let hello = serde_json::json!({ "relay_node_id": "node-x", "instance_key": "bogus" });
        control
            .send(Message::Binary(serde_json::to_vec(&hello).unwrap()))
            .await
            .unwrap();
        // A phone naming node-x is refused — the bogus gateway never registered.
        match connect_content_join(port, "node-x").await {
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
