//! The data-leg WebSocket transport.
//!
//! Each side of a relayed connection (the phone, and the gateway A) opens a
//! WebSocket to C keyed by the `relay_node_id`; [`pump_ws`] joins it to a
//! [`RelayLeg`] and shuttles binary frames in both directions, **blind** —
//! bytes are copied, never inspected (the payload is Noise-inside-TLS). The
//! pump is the single `on_upgrade` task driving a `select!` over the split
//! halves, so there's no detached writer task to starve.

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};

use crate::broker::RelayLeg;

/// Pump one WebSocket against its matched [`RelayLeg`] until either side closes:
/// binary frames the client sends go to the partner leg; frames from the
/// partner go back to the client. Non-binary control frames (ping/pong/text)
/// are ignored — the relay only carries opaque binary.
pub async fn pump_ws(socket: WebSocket, mut leg: RelayLeg) {
    let (mut sink, mut source) = socket.split();
    loop {
        tokio::select! {
            inbound = source.next() => match inbound {
                Some(Ok(Message::Binary(b))) => {
                    if leg.to_peer.send(b.to_vec()).await.is_err() {
                        break; // partner leg gone
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
            outbound = leg.from_peer.recv() => match outbound {
                Some(bytes) => {
                    if sink.send(Message::Binary(bytes.into())).await.is_err() {
                        break; // client socket gone
                    }
                }
                None => break, // partner leg closed
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::RelayBroker;
    use axum::Router;
    use axum::extract::{Path, State, WebSocketUpgrade};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::TcpStream;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::client_async;
    use tokio_tungstenite::tungstenite::Message as TMsg;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    async fn leg_handler(
        ws: WebSocketUpgrade,
        Path(key): Path<String>,
        State(broker): State<Arc<RelayBroker>>,
    ) -> impl IntoResponse {
        let leg = broker.join(&key);
        ws.on_upgrade(move |socket| pump_ws(socket, leg))
    }

    async fn connect(port: u16, key: &str) -> WebSocketStream<TcpStream> {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let req = format!("ws://127.0.0.1:{port}/leg/{key}")
            .into_client_request()
            .unwrap();
        client_async(req, stream).await.unwrap().0
    }

    async fn recv_binary(ws: &mut WebSocketStream<TcpStream>) -> Vec<u8> {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(3), ws.next())
                .await
                .expect("recv timed out")
                .expect("ws closed")
                .expect("ws error");
            match msg {
                TMsg::Binary(b) => return b,
                TMsg::Ping(_) | TMsg::Pong(_) => continue,
                other => panic!("unexpected ws frame: {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_legs_pipe_bytes_blind_both_ways() {
        let broker = Arc::new(RelayBroker::new());
        let app = Router::new()
            .route("/leg/{key}", get(leg_handler))
            .with_state(broker);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });

        // Phone + gateway both join the relay under the same node key.
        let mut phone = connect(port, "node-1").await;
        let mut gateway = connect(port, "node-1").await;

        // phone -> gateway
        phone
            .send(TMsg::Binary(b"noise-up".to_vec()))
            .await
            .unwrap();
        assert_eq!(recv_binary(&mut gateway).await, b"noise-up".to_vec());

        // gateway -> phone
        gateway
            .send(TMsg::Binary(b"noise-down".to_vec()))
            .await
            .unwrap();
        assert_eq!(recv_binary(&mut phone).await, b"noise-down".to_vec());

        server.abort();
    }
}
