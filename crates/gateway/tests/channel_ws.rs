//! Round-trip integration coverage for the gateway-side WS channel
//! server.
//!
//! Spins a real [`ChannelServer`] against a temp-dir socket, drives a
//! raw `tokio-tungstenite` WebSocket client end-to-end — register →
//! sidecar→agent message → agent→sidecar message → duplicate-register
//! rejection → disconnect cleanup.
//!
//! No Rust SDK exists any more; sidecars ship as the TypeScript
//! package under `sdks/channel-ts/`, so this test talks to the
//! gateway's WS endpoint directly via the shared [`wire`] module.

use std::sync::Arc;
use std::time::Duration;

use aura_channels::wire::{self, Frame, Message as WireMessage, PROTOCOL_VERSION};
use aura_channels::{AgentOutput, OutgoingMessage};
use aura_gateway::test_support::build_test_deps;
use aura_gateway::uds::ChannelServer;
use aura_gateway_auth::{CHANNEL_TOKEN_HEADER, ClientIdentity};
use aura_model::{ChannelType, ContentBlock, MessageMetadata};
use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const TEST_PSK: [u8; 32] = [0x42; 32];
const HANDSHAKE_URL: &str = "ws://localhost/v1/channel-ws";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn channel_ws_end_to_end() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let socket_path = tempdir.path().join("channel.sock");

    let mut tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let channel_registry = Arc::clone(&tg.deps.channel_registry);
    let channel_tokens = tg.channel_tokens.clone();
    let shutdown = tg.shutdown.clone();

    let server = ChannelServer::bind(
        &tg.deps,
        socket_path.clone(),
        TEST_PSK,
        channel_tokens.clone(),
    )
    .expect("bind ChannelServer");

    let server_shutdown = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        let _ = server.run(server_shutdown).await;
    });

    // Mint a token bound to the test process's PID. The UDS listener
    // extracts peercred on accept, the channel-auth middleware enforces
    // pid == token.pid, and the register-frame validator then re-checks
    // the same identity against the token embedded in the frame.
    let handle = channel_tokens.mint(ClientIdentity {
        pid: std::process::id(),
        label: "slack".into(),
    });
    let token = handle.token().to_string();

    let slack = ChannelType::from("slack");

    // 1. Sidecar registers.
    let mut client = connect_register(&socket_path, &token, slack.clone())
        .await
        .expect("sidecar handshake");
    assert!(
        wait_until(Duration::from_secs(2), || channel_registry
            .get(slack.clone())
            .is_some())
        .await,
        "sidecar not registered with ChannelRegistry",
    );

    // 2. Sidecar → agent message reaches router intake.
    let outbound = WireMessage {
        content: "hi aura".into(),
        session_id: "sess-1".into(),
        user_id: "user-1".into(),
        channel_type: slack.clone(),
    };
    send_frame(&mut client, &Frame::Message(outbound.clone()))
        .await
        .expect("sidecar send");

    let incoming = tokio::time::timeout(Duration::from_secs(2), tg.incoming_rx.recv())
        .await
        .expect("router intake timeout")
        .expect("router intake closed");
    assert_eq!(incoming.message.session_id, "sess-1");
    assert_eq!(incoming.message.sender.id, "user-1");
    assert_eq!(incoming.message.channel, slack);
    match incoming.message.content.first() {
        Some(ContentBlock::Text(text)) => assert_eq!(text, "hi aura"),
        other => panic!("expected text block, got {other:?}"),
    }

    // 3. Agent → sidecar delivery via the registered channel.
    let channel_handle = channel_registry
        .get(slack.clone())
        .expect("channel present after registration");
    channel_handle
        .send(AgentOutput::Message(OutgoingMessage {
            session_id: "sess-1".into(),
            channel: slack.clone(),
            content: vec![ContentBlock::Text("pong".into())],
            reply_to: None,
            metadata: MessageMetadata::default(),
        }))
        .await
        .expect("channel send");

    let recv = tokio::time::timeout(Duration::from_secs(2), recv_message(&mut client))
        .await
        .expect("sidecar recv timeout")
        .expect("sidecar recv");
    assert_eq!(recv.content, "pong");
    assert_eq!(recv.session_id, "sess-1");
    assert_eq!(recv.channel_type, slack);

    // 4. Duplicate registration for the same channel type is rejected.
    match connect_register(&socket_path, &token, slack.clone()).await {
        Ok(_) => panic!("duplicate register unexpectedly succeeded"),
        Err(ConnectError::RegistrationRejected(msg)) => {
            assert!(
                msg.contains("already registered"),
                "unexpected reason: {msg}",
            );
        }
        Err(other) => panic!("expected RegistrationRejected, got {other:?}"),
    }

    // 5. Dropping the first client tears the channel out of the registry.
    drop(channel_handle);
    drop(client);
    assert!(
        wait_until(Duration::from_secs(2), || channel_registry
            .get(slack.clone())
            .is_none())
        .await,
        "channel not cleaned up after sidecar disconnect",
    );

    // Teardown.
    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
}

type WsStream = tokio_tungstenite::WebSocketStream<UnixStream>;

#[derive(Debug)]
enum ConnectError {
    #[allow(dead_code)]
    Dial(std::io::Error),
    #[allow(dead_code)]
    Upgrade(String),
    RegistrationRejected(String),
    #[allow(dead_code)]
    ProtocolViolation(&'static str),
    #[allow(dead_code)]
    Transport(String),
    #[allow(dead_code)]
    Wire(wire::WireError),
}

impl From<wire::WireError> for ConnectError {
    fn from(e: wire::WireError) -> Self {
        ConnectError::Wire(e)
    }
}

async fn connect_register(
    socket_path: &std::path::Path,
    token: &str,
    channel_type: ChannelType,
) -> Result<WsStream, ConnectError> {
    let stream = UnixStream::connect(socket_path)
        .await
        .map_err(ConnectError::Dial)?;
    let mut request = HANDSHAKE_URL
        .into_client_request()
        .map_err(|e| ConnectError::Upgrade(e.to_string()))?;
    request.headers_mut().insert(
        CHANNEL_TOKEN_HEADER,
        token
            .parse()
            .map_err(|_| ConnectError::Upgrade("invalid token header".into()))?,
    );
    let (mut ws, _) = client_async(request, stream)
        .await
        .map_err(|e| ConnectError::Upgrade(e.to_string()))?;

    send_frame(
        &mut ws,
        &Frame::Register {
            token: token.to_string(),
            channel_type,
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .await?;

    match recv_frame(&mut ws).await? {
        Frame::RegisterAck { ok: true, .. } => Ok(ws),
        Frame::RegisterAck { ok: false, reason } => Err(ConnectError::RegistrationRejected(
            reason.unwrap_or_else(|| "no reason given".to_string()),
        )),
        _ => Err(ConnectError::ProtocolViolation("expected RegisterAck")),
    }
}

async fn send_frame(ws: &mut WsStream, frame: &Frame) -> Result<(), ConnectError> {
    let bytes = wire::encode(frame)?;
    ws.send(WsMessage::Binary(bytes))
        .await
        .map_err(|e| ConnectError::Transport(e.to_string()))
}

async fn recv_frame(ws: &mut WsStream) -> Result<Frame, ConnectError> {
    loop {
        match ws.next().await {
            None => return Err(ConnectError::Transport("peer closed".into())),
            Some(Err(e)) => return Err(ConnectError::Transport(e.to_string())),
            Some(Ok(msg)) => match msg {
                WsMessage::Binary(bytes) => return Ok(wire::decode(&bytes)?),
                WsMessage::Close(_) => return Err(ConnectError::Transport("peer closed".into())),
                WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => continue,
                WsMessage::Text(_) => {
                    return Err(ConnectError::ProtocolViolation("unexpected text frame"));
                }
            },
        }
    }
}

async fn recv_message(ws: &mut WsStream) -> Result<WireMessage, ConnectError> {
    loop {
        match recv_frame(ws).await? {
            Frame::Message(msg) => return Ok(msg),
            Frame::Delta { .. }
            | Frame::Notice { .. }
            | Frame::ApprovalRequested { .. }
            | Frame::ApprovalResolved { .. } => continue,
            Frame::Register { .. } | Frame::RegisterAck { .. } | Frame::ResolveApproval { .. } => {
                return Err(ConnectError::ProtocolViolation(
                    "unexpected frame kind post-handshake",
                ));
            }
        }
    }
}

async fn wait_until<F: Fn() -> bool>(deadline: Duration, check: F) -> bool {
    let start = tokio::time::Instant::now();
    while tokio::time::Instant::now().duration_since(start) < deadline {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    check()
}
