//! Native round-trip test for the WS + MessagePack SDK. Exercises
//! register → send → recv against a fixture server speaking the same
//! wire protocol via the server half of tokio-tungstenite.

use std::env;
use std::path::PathBuf;

use futures::{SinkExt, StreamExt};
use tempfile::NamedTempFile;
use tokio::net::UnixListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::client::{Client, ENV_CHANNEL_SOCKET, ENV_CHANNEL_TOKEN};
use super::wire::{self, ChannelTypeV2, Frame, Message};

#[tokio::test]
async fn register_and_roundtrip() {
    let socket_path = {
        // Create and drop a tempfile to reserve a unique path the
        // UnixListener can bind to.
        let tf = NamedTempFile::new().unwrap();
        let path: PathBuf = tf.path().to_path_buf();
        drop(tf);
        path
    };

    // SAFETY: set_var is `unsafe` in Rust 2024. Confined to this test;
    // no other test in this module touches the same env vars.
    unsafe {
        env::set_var(ENV_CHANNEL_SOCKET, &socket_path);
        env::set_var(ENV_CHANNEL_TOKEN, "secret-token");
    }

    let listener = UnixListener::bind(&socket_path).unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let ws = accept_async(stream).await.unwrap();
        let (mut sink, mut source) = ws.split();

        // Expect Register frame with the token we set.
        let first = source.next().await.unwrap().unwrap();
        let bytes = match first {
            WsMessage::Binary(b) => b,
            other => panic!("expected binary frame, got {:?}", other),
        };
        let frame = wire::decode(&bytes).unwrap();
        match frame {
            Frame::Register {
                token,
                channel_type,
                protocol_version,
            } => {
                assert_eq!(token, "secret-token");
                assert_eq!(channel_type, ChannelTypeV2("slack".into()));
                assert_eq!(protocol_version, wire::PROTOCOL_VERSION);
            }
            other => panic!("expected Register, got {:?}", other),
        }

        // Send RegisterAck.
        let ack = wire::encode(&Frame::RegisterAck {
            ok: true,
            reason: None,
        })
        .unwrap();
        sink.send(WsMessage::Binary(ack)).await.unwrap();

        // Echo the next Message frame back.
        let next = source.next().await.unwrap().unwrap();
        let echo_bytes = match next {
            WsMessage::Binary(b) => b,
            other => panic!("expected binary Message frame, got {:?}", other),
        };
        sink.send(WsMessage::Binary(echo_bytes)).await.unwrap();
    });

    let client = Client::connect(ChannelTypeV2("slack".into()))
        .await
        .unwrap();

    let msg = Message {
        content: "hello aura".into(),
        session_id: "sess-1".into(),
        user_id: "user-1".into(),
        channel_type: ChannelTypeV2("slack".into()),
    };
    client.send(msg.clone()).await.unwrap();

    let received = client.recv().await.unwrap();
    assert_eq!(received, msg);

    server.await.unwrap();

    // Cleanup the socket file we bound.
    let _ = std::fs::remove_file(&socket_path);
}
