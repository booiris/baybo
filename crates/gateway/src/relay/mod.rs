//! The gateway's outbound **A↔C control connection**.
//!
//! A NAT'd gateway can't be dialed by the remote host (C), so A instead holds a
//! **persistent outbound control connection** to C. On open it sends a
//! [`ControlClientHello`] (its `relay_node_id` + its per-instance admission
//! key); thereafter C pushes [`ControlServerMsg`] signals — today only
//! `OpenDataLeg`, meaning a phone is waiting at the relay and A should open a
//! data leg to meet it.
//!
//! This module is the **protocol-over-WebSocket core**: it runs over an
//! already-connected stream, so it is host-testable against a mock C. The
//! production wrapper supplies the TCP/TLS dial of `relay.base_url` and a
//! reconnect loop; data-leg establishment + the blind byte-pipe ride on top
//! (the relay path is operated but content prefers direct in phase 1).

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// Sent once by A right after the control WS opens. Identifies the gateway by
/// its C-assigned `relay_node_id` and authenticates with its admission key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ControlClientHello {
    pub relay_node_id: String,
    pub instance_key: String,
}

/// A control-plane message C pushes to A. `OpenDataLeg` means a phone is parked
/// at the relay under `relay_key`; A opens a data leg and joins to meet it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ControlServerMsg {
    OpenDataLeg { relay_key: String },
}

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
    hello: &ControlClientHello,
    signals: mpsc::Sender<ControlServerMsg>,
) -> Result<(), ControlError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = url
        .into_client_request()
        .map_err(|e| ControlError::Codec(format!("bad url: {e}")))?;
    let (mut ws, _) = client_async(request, stream).await?;

    let hello_bytes = serde_json::to_vec(hello).map_err(|e| ControlError::Codec(e.to_string()))?;
    ws.send(Message::Binary(hello_bytes)).await?;

    while let Some(msg) = ws.next().await {
        match msg? {
            Message::Binary(b) => match serde_json::from_slice::<ControlServerMsg>(&b) {
                Ok(sig) => {
                    if signals.send(sig).await.is_err() {
                        break; // the consumer dropped its receiver
                    }
                }
                Err(e) => tracing::warn!(error = %e, "unparseable control signal; ignoring"),
            },
            Message::Close(_) => break,
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::ws::{Message as AxumMsg, WebSocket, WebSocketUpgrade};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::Router;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use tokio::net::TcpStream;

    // The mock C records the hello it received and emits one OpenDataLeg.
    #[derive(Clone, Default)]
    struct MockC {
        received_hello: Arc<Mutex<Option<ControlClientHello>>>,
    }

    async fn mock_c_handler(
        ws: WebSocketUpgrade,
        axum::extract::State(state): axum::extract::State<MockC>,
    ) -> impl IntoResponse {
        ws.on_upgrade(move |socket| run_mock_c(socket, state))
    }

    async fn run_mock_c(mut socket: WebSocket, state: MockC) {
        if let Some(Ok(AxumMsg::Binary(b))) = socket.recv().await
            && let Ok(hello) = serde_json::from_slice::<ControlClientHello>(&b)
        {
            *state.received_hello.lock() = Some(hello);
            let sig = ControlServerMsg::OpenDataLeg {
                relay_key: "leg-xyz".into(),
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
        let hello = ControlClientHello {
            relay_node_id: "node-1".into(),
            instance_key: "inst-A".into(),
        };
        let (tx, mut rx) = mpsc::channel(4);
        let client = tokio::spawn(async move {
            run_control_connection(&url, stream, &hello, tx).await
        });

        // A receives the OpenDataLeg signal C pushed.
        let sig = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("signal timed out")
            .expect("signal channel closed");
        assert_eq!(
            sig,
            ControlServerMsg::OpenDataLeg {
                relay_key: "leg-xyz".into(),
            },
        );

        // C received A's hello (relay_node_id + instance key).
        assert_eq!(
            *mock.received_hello.lock(),
            Some(ControlClientHello {
                relay_node_id: "node-1".into(),
                instance_key: "inst-A".into(),
            }),
        );

        client.abort();
        server.abort();
    }

    #[test]
    fn open_data_leg_wire_shape_is_stable() {
        let sig = ControlServerMsg::OpenDataLeg {
            relay_key: "k".into(),
        };
        let v: serde_json::Value = serde_json::to_value(&sig).unwrap();
        assert_eq!(v["t"], "open_data_leg");
        assert_eq!(v["relay_key"], "k");
    }
}
