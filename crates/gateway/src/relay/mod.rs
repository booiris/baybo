//! The gateway's outbound **A↔C control connection**.
//!
//! A NAT'd gateway can't be dialed by the remote host (C), so A instead holds a
//! **persistent outbound control connection** to C. On open it names itself with a
//! [`ControlHello`] (its `relay_node_id`); its `remote_api_key` rides the
//! `x-remote-api-key` dial header (the shared admission pre-layer), not the hello.
//! Thereafter C pushes [`ControlSignal`] signals — today only
//! `OpenDataLeg`, meaning a phone is waiting at the relay and A should open a
//! data leg to meet it.
//!
//! This module is the **protocol-over-WebSocket core**: it runs over an
//! already-connected stream, so it is host-testable against a mock C. The
//! production wrapper supplies the TCP/TLS dial of `relay.base_url` and a
//! reconnect loop; data-leg establishment and the blind byte-pipe ride on top.

use std::time::Duration;

use baybo_security::SecretVault;
use futures::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{WebSocketStream, client_async, connect_async};

/// `SecretVault` key holding A's stable, non-secret `relay_node_id` (a UUID).
/// The app caches it at pairing to reach a NAT'd A via the relay, so it must not
/// change underneath an already-paired device — hence it's persisted, not
/// re-derived each boot.
const RELAY_NODE_ID_VAULT_KEY: &str = "relay.node_id";

/// Keepalive cadence on the A→C control connection (and its missed-Pong death
/// timeout): A pings C every interval; C auto-Pongs, so a missed answer by the
/// next tick means the connection is half-open.
const CONTROL_KEEPALIVE: Duration = Duration::from_secs(30);

/// Serializes the first-run mint of [`RELAY_NODE_ID_VAULT_KEY`]. Without it the
/// boot control connection and a concurrent first pairing could each read an
/// empty vault and mint *different* ids (last-writer-wins on store), leaving the
/// id A registered under ≠ the id it advertised. Cheap: contended only once, on
/// the first ever call before the value is persisted.
static RELAY_NODE_ID_MINT: Mutex<()> = Mutex::const_new(());

/// Load A's persisted `relay_node_id`, generating and storing one on first use.
/// The gateway presents it on the control connection and advertises it in
/// `GatewayWelcome`; both sides must agree, so both read it through here.
pub async fn load_or_create_relay_node_id(vault: &SecretVault) -> anyhow::Result<String> {
    if let Some(id) = read_relay_node_id(vault).await? {
        return Ok(id);
    }
    // Double-checked under the mint lock so concurrent first callers agree on one.
    let _guard = RELAY_NODE_ID_MINT.lock().await;
    if let Some(id) = read_relay_node_id(vault).await? {
        return Ok(id);
    }
    let id = uuid::Uuid::new_v4().to_string();
    vault
        .store_secret(RELAY_NODE_ID_VAULT_KEY, id.as_bytes())
        .await
        .map_err(|e| anyhow::anyhow!("vault store {RELAY_NODE_ID_VAULT_KEY}: {e}"))?;
    Ok(id)
}

async fn read_relay_node_id(vault: &SecretVault) -> anyhow::Result<Option<String>> {
    let Some(secret) = vault
        .get_secret(RELAY_NODE_ID_VAULT_KEY)
        .await
        .map_err(|e| anyhow::anyhow!("vault get {RELAY_NODE_ID_VAULT_KEY}: {e}"))?
    else {
        return Ok(None);
    };
    let id = String::from_utf8_lossy(secret.as_bytes()).into_owned();
    Ok((!id.is_empty()).then_some(id))
}

/// The control-plane wire types ([`ControlHello`] A sends, [`ControlSignal`] C
/// pushes) live in the shared protocol crate, so A and C agree by construction.
pub use remote_host_protocol::relay::{ControlHello, ControlSignal, LegClass};

#[derive(Debug, Error)]
pub enum ControlError {
    #[error("ws: {0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("codec: {0}")]
    Codec(String),
}

/// Run the A-side control connection over an already-connected `stream` (the
/// caller dials TCP + TLS for `wss://`; tests pass a plain `ws://` TcpStream):
/// complete the WS handshake, send `hello`, then forward every parsed server
/// signal to `signals` until the connection closes. Unparseable frames are
/// logged and skipped (forward-compatible with future signal kinds).
pub async fn run_control_connection<S>(
    url: &str,
    stream: S,
    hello: &ControlHello,
    signals: mpsc::Sender<ControlSignal>,
) -> Result<(), ControlError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = url
        .into_client_request()
        .map_err(|e| ControlError::Codec(format!("bad url: {e}")))?;
    let (ws, _) = client_async(request, stream).await?;
    pump_control(ws, hello, signals).await
}

/// Production dial: connect to C's control endpoint (`wss://…` TLS handled by
/// `connect_async`) and run the control loop, returning when the connection
/// closes or errors so the caller can reconnect.
pub async fn connect_control(
    url: &str,
    remote_api_key: &str,
    hello: &ControlHello,
    signals: mpsc::Sender<ControlSignal>,
) -> Result<(), ControlError> {
    let mut request = url
        .into_client_request()
        .map_err(|e| ControlError::Codec(format!("bad url: {e}")))?;
    // Admission rides the dial header now (the relay's shared pre-layer), so the
    // hello carries only the relay_node_id.
    let value = remote_api_key
        .parse()
        .map_err(|e| ControlError::Codec(format!("bad instance key header: {e}")))?;
    request
        .headers_mut()
        .insert(remote_host_protocol::relay::REMOTE_API_KEY_HEADER, value);
    let (ws, _) = connect_async(request).await?;
    pump_control(ws, hello, signals).await
}

/// The control protocol over an already-handshaken WS (shared by the
/// stream-based [`run_control_connection`] and the production [`connect_control`]
/// so both speak it identically): send `hello`, then forward every parsed server
/// signal to `signals` until the connection closes. Unparseable frames are
/// logged and skipped (forward-compatible with future signal kinds).
async fn pump_control<T>(
    mut ws: WebSocketStream<T>,
    hello: &ControlHello,
    signals: mpsc::Sender<ControlSignal>,
) -> Result<(), ControlError>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let hello_bytes = serde_json::to_vec(hello).map_err(|e| ControlError::Codec(e.to_string()))?;
    ws.send(Message::Binary(hello_bytes)).await?;

    // The control connection is long-lived and idle by nature (signals only
    // arrive when a phone shows up), so a half-open TCP connection (NAT rebind,
    // silent firewall drop) would otherwise hang `ws.next()` forever and freeze
    // the reconnect loop. Send a keepalive Ping each interval; if the prior one
    // drew no frame back (C auto-Pongs, so a healthy peer always answers), treat
    // the peer as gone and bail so the caller re-dials.
    let mut keepalive = tokio::time::interval_at(
        tokio::time::Instant::now() + CONTROL_KEEPALIVE,
        CONTROL_KEEPALIVE,
    );
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut awaiting_pong = false;

    loop {
        tokio::select! {
            msg = ws.next() => {
                let Some(msg) = msg else { break };
                match msg? {
                    Message::Binary(b) => {
                        awaiting_pong = false;
                        match serde_json::from_slice::<ControlSignal>(&b) {
                            Ok(sig) => {
                                if signals.send(sig).await.is_err() {
                                    break; // the consumer dropped its receiver
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "unparseable control signal; ignoring")
                            }
                        }
                    }
                    Message::Close(_) => break,
                    // Any other frame (Pong to our keepalive, an unsolicited
                    // Ping) is liveness proof.
                    _ => awaiting_pong = false,
                }
            }
            _ = keepalive.tick() => {
                if awaiting_pong {
                    return Err(ControlError::Codec("control keepalive unanswered".into()));
                }
                ws.send(Message::Ping(Vec::new())).await?;
                awaiting_pong = true;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::extract::ws::{Message as AxumMsg, WebSocket, WebSocketUpgrade};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use tokio::net::TcpStream;

    // The mock C records the hello it received and emits one OpenDataLeg.
    #[derive(Clone, Default)]
    struct MockC {
        received_hello: Arc<Mutex<Option<ControlHello>>>,
    }

    async fn mock_c_handler(
        ws: WebSocketUpgrade,
        axum::extract::State(state): axum::extract::State<MockC>,
    ) -> impl IntoResponse {
        ws.on_upgrade(move |socket| run_mock_c(socket, state))
    }

    async fn run_mock_c(mut socket: WebSocket, state: MockC) {
        if let Some(Ok(AxumMsg::Binary(b))) = socket.recv().await
            && let Ok(hello) = serde_json::from_slice::<ControlHello>(&b)
        {
            *state.received_hello.lock() = Some(hello);
            let sig = ControlSignal::OpenDataLeg {
                relay_key: "leg-xyz".into(),
                class: LegClass::Chat,
            };
            let _ = socket
                .send(AxumMsg::Binary(serde_json::to_vec(&sig).unwrap().into()))
                .await;
        }
        // Keep the socket open briefly so the signal flushes before close.
        let _ = socket.recv().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn control_client_sends_hello_and_receives_open_data_leg() {
        let mock = MockC::default();
        let app = Router::new()
            .route("/control", get(mock_c_handler))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });

        // A dials the control WS (ws:// for the test; production wss:// adds TLS).
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let url = format!("ws://127.0.0.1:{port}/control");
        let hello = ControlHello {
            relay_node_id: "node-1".into(),
        };
        let (tx, mut rx) = mpsc::channel(4);
        let client =
            tokio::spawn(async move { run_control_connection(&url, stream, &hello, tx).await });

        // A receives the OpenDataLeg signal C pushed.
        let sig = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("signal timed out")
            .expect("signal channel closed");
        assert_eq!(
            sig,
            ControlSignal::OpenDataLeg {
                relay_key: "leg-xyz".into(),
                class: LegClass::Chat,
            },
        );

        // C received A's hello (relay_node_id + instance key).
        assert_eq!(
            *mock.received_hello.lock(),
            Some(ControlHello {
                relay_node_id: "node-1".into(),
            }),
        );

        client.abort();
        server.abort();
    }

    #[test]
    fn open_data_leg_wire_shape_is_stable() {
        let sig = ControlSignal::OpenDataLeg {
            relay_key: "k".into(),
            class: LegClass::Chat,
        };
        let v: serde_json::Value = serde_json::to_value(&sig).unwrap();
        assert_eq!(v["t"], "open_data_leg");
        assert_eq!(v["relay_key"], "k");
        assert_eq!(v["class"], "chat");
    }
}
