//! Round-trip integration coverage for the gateway-side WS channel
//! server.
//!
//! Spins a real [`ChannelServer`] on an ephemeral loopback TCP port,
//! drives a raw `tokio-tungstenite` WebSocket client end-to-end —
//! register → sidecar→agent message → agent→sidecar message →
//! duplicate-register rejection → disconnect cleanup.
//!
//! No Rust SDK exists any more; sidecars ship as the TypeScript
//! package under `sdks/channel-ts/`, so this test talks to the
//! gateway's WS endpoint directly via the shared [`wire`] module.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use aura_channels::wire::{self, Frame, Message as WireMessage, PROTOCOL_VERSION};
use aura_channels::{AgentOutput, OutgoingMessage};
use aura_gateway::channel_listener::ChannelServer;
use aura_gateway::test_support::build_test_deps;
use aura_gateway::{
    CHANNEL_TOKEN_HEADER, ChannelTokenTable, ClientIdentity, TUI_CLIENT_LABEL, TokenHandle,
};
use aura_model::{ChannelType, ContentBlock, MessageMetadata};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// Mint a fresh TUI-flavoured token in `tokens` and return the live
/// `(token_string, handle)` pair. The handle revokes the token on
/// drop, so callers should keep it alive for the duration of the test.
fn mint_test_tui_token(tokens: &ChannelTokenTable) -> (String, TokenHandle) {
    let handle = tokens.mint(ClientIdentity {
        pid: 0,
        label: TUI_CLIENT_LABEL.to_string(),
        bound_channel_type: None,
    });
    (handle.token().to_string(), handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn channel_ws_end_to_end() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let port_file =
        aura_workspace::WorkspacePaths::new(tempdir.path().to_path_buf()).channel_port();

    let mut tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let channel_registry = Arc::clone(&tg.deps.channel_registry);
    let channel_tokens = tg.channel_tokens.clone();
    let pairing_store = tg.deps.stores.channel_pairing.clone();
    let shutdown = tg.shutdown.clone();

    let server = ChannelServer::bind(&tg.deps, port_file.clone(), channel_tokens.clone())
        .expect("bind ChannelServer");
    let port = server.port();

    let server_shutdown = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        let _ = server.run(server_shutdown).await;
    });

    // Mint a token. Peer credentials are no longer part of the auth
    // chain (loopback TCP), so any PID works — we use the test
    // process's id purely for diagnostic symmetry with production.
    let handle = channel_tokens.mint(ClientIdentity {
        pid: std::process::id(),
        label: "slack".into(),
        bound_channel_type: Some("slack".into()),
    });
    let token = handle.token().to_string();

    let slack = ChannelType::from("slack");

    // Pre-approve a pairing for (slack, prod-bot, user-1) so the
    // resolver-driven path admits the inbound message immediately
    // without an operator round-trip.
    let now = chrono::Utc::now().timestamp();
    let row = pairing_store
        .upsert_pending(&slack, "prod-bot", "user-1", "TESTCD", now, now + 600)
        .await
        .expect("seed pending pairing");
    pairing_store
        .approve_by_code(&row.code, now)
        .await
        .expect("approve pairing");

    // 1. Sidecar registers.
    let mut client = connect_register(port, &token, slack.clone())
        .await
        .expect("sidecar handshake");
    assert!(
        wait_until(Duration::from_secs(2), || channel_registry
            .get_sidecar(slack.clone())
            .is_some())
        .await,
        "sidecar not registered with ChannelRegistry",
    );

    // 2. Sidecar → agent message reaches router intake. session_id
    // intentionally left empty: subprocess sidecars are no longer
    // permitted to self-supply the session id (would bypass pairing
    // and let the sidecar inject into other users' sessions).
    let outbound = WireMessage {
        content: "hi aura".into(),
        session_id: String::new(),
        user_id: "user-1".into(),
        channel_type: slack.clone(),
        bot_id: "prod-bot".into(),
        attachments: Vec::new(),
        platform_msg_id: String::new(),
    };
    send_frame(&mut client, &Frame::Message(outbound.clone()))
        .await
        .expect("sidecar send");

    let incoming = tokio::time::timeout(Duration::from_secs(2), tg.incoming_rx.recv())
        .await
        .expect("router intake timeout")
        .expect("router intake closed");
    let resolved_session_id = incoming.message.session_id.clone();
    assert!(
        !resolved_session_id.is_empty(),
        "resolver should have allocated a session id",
    );
    assert_eq!(incoming.message.sender.id, "user-1");
    assert_eq!(incoming.message.channel, slack);
    match incoming.message.content.first() {
        Some(ContentBlock::Text(text)) => assert_eq!(text, "hi aura"),
        other => panic!("expected text block, got {other:?}"),
    }

    // 3. Agent → sidecar delivery via the registered channel.
    let channel_handle = channel_registry
        .get_sidecar(slack.clone())
        .expect("channel present after registration");
    channel_handle
        .send(AgentOutput::Message(OutgoingMessage {
            session_id: resolved_session_id.clone(),
            user_id: "user-1".into(),
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
    assert_eq!(recv.session_id, resolved_session_id);
    assert_eq!(recv.channel_type, slack);

    // 4. Duplicate registration for the same channel type is rejected.
    match connect_register(port, &token, slack.clone()).await {
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
            .get_sidecar(slack.clone())
            .is_none())
        .await,
        "channel not cleaned up after sidecar disconnect",
    );

    // Teardown.
    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
}

type WsStream = tokio_tungstenite::WebSocketStream<TcpStream>;

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

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

fn handshake_url(port: u16) -> String {
    format!("ws://127.0.0.1:{port}/v1/channel-ws")
}

async fn connect_register(
    port: u16,
    token: &str,
    channel_type: ChannelType,
) -> Result<WsStream, ConnectError> {
    let stream = TcpStream::connect(loopback(port))
        .await
        .map_err(ConnectError::Dial)?;
    let mut request = handshake_url(port)
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
            session_id: None,
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
            | Frame::ApprovalResolved { .. }
            | Frame::HistorySnapshot { .. }
            | Frame::StartBot { .. }
            | Frame::StopBot { .. }
            | Frame::SlashManifest { .. } => continue,
            Frame::Register { .. }
            | Frame::RegisterAck { .. }
            | Frame::ResolveApproval { .. }
            | Frame::HistoryAppend { .. }
            | Frame::SidecarLog { .. }
            | Frame::BotStatus { .. } => {
                return Err(ConnectError::ProtocolViolation(
                    "unexpected frame kind post-handshake",
                ));
            }
        }
    }
}

/// Receive the next `Notice` frame. Used by the pairing-gate test
/// which expects aura to push a refusal notice in response to an
/// un-paired inbound.
async fn recv_notice(ws: &mut WsStream) -> Result<(String, String, String), ConnectError> {
    loop {
        match recv_frame(ws).await? {
            Frame::Notice {
                user_id,
                level,
                text,
                ..
            } => return Ok((user_id, level, text)),
            Frame::Message(_) | Frame::Delta { .. } => continue,
            Frame::ApprovalRequested { .. }
            | Frame::ApprovalResolved { .. }
            | Frame::HistorySnapshot { .. }
            | Frame::StartBot { .. }
            | Frame::StopBot { .. }
            | Frame::SlashManifest { .. } => continue,
            Frame::Register { .. }
            | Frame::RegisterAck { .. }
            | Frame::ResolveApproval { .. }
            | Frame::HistoryAppend { .. }
            | Frame::SidecarLog { .. }
            | Frame::BotStatus { .. } => {
                return Err(ConnectError::ProtocolViolation(
                    "unexpected frame kind post-handshake",
                ));
            }
        }
    }
}

/// Pairing gate end-to-end: an un-paired sidecar-originated Message
/// gets a Notice back with a 6-char code and is not forwarded to the
/// router. Operator-side approval via the store flips the triple to
/// approved; the next message flows through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pairing_gate_rejects_unpaired_then_admits_after_approve() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let port_file =
        aura_workspace::WorkspacePaths::new(tempdir.path().to_path_buf()).channel_port();

    let mut tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let pairing_store = tg.deps.stores.channel_pairing.clone();
    let channel_tokens = tg.channel_tokens.clone();
    let shutdown = tg.shutdown.clone();

    let server = ChannelServer::bind(&tg.deps, port_file.clone(), channel_tokens.clone())
        .expect("bind ChannelServer");
    let port = server.port();
    let server_shutdown = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        let _ = server.run(server_shutdown).await;
    });

    let handle = channel_tokens.mint(ClientIdentity {
        pid: std::process::id(),
        label: "slack".into(),
        bound_channel_type: Some("slack".into()),
    });
    let token = handle.token().to_string();
    let slack = ChannelType::from("slack");

    let mut client = connect_register(port, &token, slack.clone())
        .await
        .expect("sidecar handshake");

    // 1. Un-paired triple — aura refuses with a Notice containing a code.
    let inbound = WireMessage {
        content: "hi".into(),
        // Empty session_id triggers the empty-session branch that
        // runs the pairing gate; the sidecar path also carries
        // user_id + bot_id.
        session_id: String::new(),
        user_id: "alice".into(),
        channel_type: slack.clone(),
        bot_id: "prod-bot".into(),
        attachments: Vec::new(),
        platform_msg_id: String::new(),
    };
    send_frame(&mut client, &Frame::Message(inbound.clone()))
        .await
        .expect("send inbound");

    let (notice_user, level, text) =
        tokio::time::timeout(Duration::from_secs(2), recv_notice(&mut client))
            .await
            .expect("notice timeout")
            .expect("notice recv");
    assert_eq!(notice_user, "alice");
    assert_eq!(level, "warn");
    assert!(text.contains("Pairing required"), "notice text: {text}");

    // Router intake must NOT have seen the message.
    assert!(
        tokio::time::timeout(Duration::from_millis(100), tg.incoming_rx.recv())
            .await
            .is_err(),
        "un-paired message reached the router intake",
    );

    // A pending row exists with the code from the Notice.
    let row = pairing_store
        .get(&slack, "prod-bot", "alice")
        .await
        .expect("get pairing")
        .expect("pending row present");
    assert_eq!(row.status, aura_storage::PairingStatus::Pending);
    assert!(text.contains(&row.code), "Notice didn't echo the code");

    // 2. Operator approves via the code.
    let now = chrono::Utc::now().timestamp();
    let approved = pairing_store
        .approve_by_code(&row.code, now)
        .await
        .expect("approve")
        .expect("approved row");
    assert_eq!(approved.status, aura_storage::PairingStatus::Approved);

    // 3. Same triple now makes it through to the router.
    send_frame(&mut client, &Frame::Message(inbound.clone()))
        .await
        .expect("send after approve");
    let incoming = tokio::time::timeout(Duration::from_secs(2), tg.incoming_rx.recv())
        .await
        .expect("intake timeout")
        .expect("intake closed");
    assert_eq!(incoming.message.sender.id, "alice");

    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
}

/// `/new` end-to-end for a sidecar channel: the gateway intercepts
/// the slash command, repoints the `(channel_type, user_id) →
/// session_id` mapping to a fresh aura session, and replies on the
/// same WS with a confirmation Frame::Message. The next ordinary
/// inbound from the same user must resolve to a *different* session
/// than the pre-/new mapping. This exercises the route.rs
/// `try_handle` integration that the unit tests in
/// `channel::slash::tests` cannot reach.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slash_new_resets_session_for_sidecar() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let port_file =
        aura_workspace::WorkspacePaths::new(tempdir.path().to_path_buf()).channel_port();

    let mut tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let channel_tokens = tg.channel_tokens.clone();
    let pairing_store = tg.deps.stores.channel_pairing.clone();
    let shutdown = tg.shutdown.clone();

    let server = ChannelServer::bind(&tg.deps, port_file.clone(), channel_tokens.clone())
        .expect("bind ChannelServer");
    let port = server.port();
    let server_shutdown = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        let _ = server.run(server_shutdown).await;
    });

    let weixin = ChannelType::weixin();

    let handle = channel_tokens.mint(ClientIdentity {
        pid: std::process::id(),
        label: "weixin".into(),
        bound_channel_type: Some(ChannelType::WEIXIN.into()),
    });
    let token = handle.token().to_string();

    // Pre-approve the (weixin, prod-bot, user-1) triple so the
    // pairing gate doesn't intercept us.
    let now = chrono::Utc::now().timestamp();
    let row = pairing_store
        .upsert_pending(&weixin, "prod-bot", "user-1", "TESTCD", now, now + 600)
        .await
        .expect("seed pending pairing");
    pairing_store
        .approve_by_code(&row.code, now)
        .await
        .expect("approve pairing");

    let mut client = connect_register(port, &token, weixin.clone())
        .await
        .expect("sidecar handshake");

    // 1. Establish an initial session by sending a normal inbound.
    let initial = WireMessage {
        content: "hi".into(),
        session_id: String::new(),
        user_id: "user-1".into(),
        channel_type: weixin.clone(),
        bot_id: "prod-bot".into(),
        attachments: Vec::new(),
        platform_msg_id: String::new(),
    };
    send_frame(&mut client, &Frame::Message(initial.clone()))
        .await
        .expect("send initial");
    let first = tokio::time::timeout(Duration::from_secs(2), tg.incoming_rx.recv())
        .await
        .expect("intake timeout")
        .expect("intake closed");
    let first_session = first.message.session_id.clone();
    assert!(!first_session.is_empty(), "first session id missing");

    // 2. Sidecar forwards `/new` from the user. The gateway must NOT
    //    deliver this to the router intake; it must echo a Message
    //    confirmation back on the same WS instead.
    let slash = WireMessage {
        content: "/new".into(),
        session_id: String::new(),
        user_id: "user-1".into(),
        channel_type: weixin.clone(),
        bot_id: "prod-bot".into(),
        attachments: Vec::new(),
        platform_msg_id: String::new(),
    };
    send_frame(&mut client, &Frame::Message(slash))
        .await
        .expect("send /new");

    let reply = tokio::time::timeout(Duration::from_secs(2), recv_message(&mut client))
        .await
        .expect("slash reply timeout")
        .expect("slash reply recv");
    assert_eq!(reply.user_id, "user-1");
    assert_eq!(reply.channel_type, weixin);
    assert!(
        reply.content.contains("fresh session"),
        "unexpected reply content: {}",
        reply.content,
    );
    assert_ne!(reply.session_id, "");
    assert_ne!(
        reply.session_id, first_session,
        "/new must mint a new aura session, not return the old one",
    );

    // Router intake must NOT have received the `/new` itself.
    assert!(
        tokio::time::timeout(Duration::from_millis(100), tg.incoming_rx.recv())
            .await
            .is_err(),
        "/new leaked past the slash dispatcher into the router intake",
    );

    // 3. The next ordinary inbound from the same user must resolve to
    //    the *new* session id, proving the channel mapping was
    //    repointed (not just that the slash reply alone happened).
    let after = WireMessage {
        content: "hi again".into(),
        session_id: String::new(),
        user_id: "user-1".into(),
        channel_type: weixin.clone(),
        bot_id: "prod-bot".into(),
        attachments: Vec::new(),
        platform_msg_id: String::new(),
    };
    send_frame(&mut client, &Frame::Message(after))
        .await
        .expect("send post-/new inbound");
    let second = tokio::time::timeout(Duration::from_secs(2), tg.incoming_rx.recv())
        .await
        .expect("post-/new intake timeout")
        .expect("intake closed");
    assert_ne!(
        second.message.session_id, first_session,
        "post-/new inbound still resolved to the old session",
    );
    assert_eq!(second.message.session_id, reply.session_id);

    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
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

/// Two concurrent TUI processes on one gateway, pinned to different
/// session ids. Each receives only its own session's output and
/// disconnecting one leaves the other intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_tui_clients_same_gateway_different_sessions() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let port_file =
        aura_workspace::WorkspacePaths::new(tempdir.path().to_path_buf()).channel_port();

    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let channel_registry = Arc::clone(&tg.deps.channel_registry);
    let channel_tokens = tg.channel_tokens.clone();
    let shutdown = tg.shutdown.clone();

    let server = ChannelServer::bind(&tg.deps, port_file.clone(), channel_tokens.clone())
        .expect("bind ChannelServer");
    let port = server.port();

    let server_shutdown = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        let _ = server.run(server_shutdown).await;
    });

    let (tui_token, _tui_handle) = mint_test_tui_token(&channel_tokens);

    let tui = ChannelType::from(ChannelType::TUI);

    // Two TUIs pin distinct session ids.
    let mut alice = connect_register_tui(port, &tui_token, "sess-alice")
        .await
        .expect("alice handshake");
    let mut bob = connect_register_tui(port, &tui_token, "sess-bob")
        .await
        .expect("bob handshake");

    assert!(
        wait_until(Duration::from_secs(2), || {
            let clients = channel_registry.list_session_clients();
            clients.iter().any(|s| s == "sess-alice") && clients.iter().any(|s| s == "sess-bob")
        })
        .await,
        "both session-scoped TUI clients should be registered",
    );

    // Duplicate registration of an already-claimed session id is rejected.
    match connect_register_tui(port, &tui_token, "sess-alice").await {
        Ok(_) => panic!("duplicate session register unexpectedly succeeded"),
        Err(ConnectError::RegistrationRejected(msg)) => {
            assert!(msg.contains("already"), "unexpected reason: {msg}",);
        }
        Err(other) => panic!("expected RegistrationRejected, got {other:?}"),
    }

    // Route a message to Alice's session and verify only Alice sees it.
    let alice_channel = channel_registry
        .get_for(&tui, "sess-alice")
        .expect("alice channel present");
    alice_channel
        .send(AgentOutput::Message(OutgoingMessage {
            session_id: "sess-alice".into(),
            user_id: "alice".into(),
            channel: tui.clone(),
            content: vec![ContentBlock::Text("hello alice".into())],
            reply_to: None,
            metadata: MessageMetadata::default(),
        }))
        .await
        .expect("send to alice");

    let got = tokio::time::timeout(Duration::from_secs(2), recv_message(&mut alice))
        .await
        .expect("alice recv timeout")
        .expect("alice recv");
    assert_eq!(got.content, "hello alice");
    assert_eq!(got.session_id, "sess-alice");

    // Same routing call for Bob picks a different channel handle.
    let bob_channel = channel_registry
        .get_for(&tui, "sess-bob")
        .expect("bob channel present");
    assert!(
        !Arc::ptr_eq(&alice_channel, &bob_channel),
        "alice and bob must resolve to distinct channel handles",
    );
    bob_channel
        .send(AgentOutput::Message(OutgoingMessage {
            session_id: "sess-bob".into(),
            user_id: "bob".into(),
            channel: tui.clone(),
            content: vec![ContentBlock::Text("hey bob".into())],
            reply_to: None,
            metadata: MessageMetadata::default(),
        }))
        .await
        .expect("send to bob");

    let got = tokio::time::timeout(Duration::from_secs(2), recv_message(&mut bob))
        .await
        .expect("bob recv timeout")
        .expect("bob recv");
    assert_eq!(got.content, "hey bob");
    assert_eq!(got.session_id, "sess-bob");

    // Drop Alice. Bob should remain registered and functional.
    drop(alice_channel);
    drop(alice);
    assert!(
        wait_until(Duration::from_secs(2), || {
            let clients = channel_registry.list_session_clients();
            !clients.iter().any(|s| s == "sess-alice") && clients.iter().any(|s| s == "sess-bob")
        })
        .await,
        "alice should be cleaned up while bob remains",
    );

    bob_channel
        .send(AgentOutput::Message(OutgoingMessage {
            session_id: "sess-bob".into(),
            user_id: "bob".into(),
            channel: tui.clone(),
            content: vec![ContentBlock::Text("still there?".into())],
            reply_to: None,
            metadata: MessageMetadata::default(),
        }))
        .await
        .expect("send to bob after alice drop");
    let got = tokio::time::timeout(Duration::from_secs(2), recv_message(&mut bob))
        .await
        .expect("bob recv-2 timeout")
        .expect("bob recv-2");
    assert_eq!(got.content, "still there?");

    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
}

/// Server-side history acts like a zsh ring shared across every TUI
/// process attached to this gateway: entries one TUI writes become
/// part of the snapshot a subsequent TUI sees, with consecutive
/// duplicates deduped and no vault contention between the two
/// clients.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tui_history_round_trips_across_clients() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let port_file =
        aura_workspace::WorkspacePaths::new(tempdir.path().to_path_buf()).channel_port();

    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let channel_tokens = tg.channel_tokens.clone();
    let shutdown = tg.shutdown.clone();

    let server = ChannelServer::bind(&tg.deps, port_file.clone(), channel_tokens.clone())
        .expect("bind ChannelServer");
    let port = server.port();

    let server_shutdown = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        let _ = server.run(server_shutdown).await;
    });

    let (tui_token, _tui_handle) = mint_test_tui_token(&channel_tokens);

    // Alice connects first — fresh vault, empty snapshot.
    let (mut alice, alice_snapshot) =
        connect_register_tui_with_snapshot(port, &tui_token, "sess-alice")
            .await
            .expect("alice handshake");
    assert!(alice_snapshot.is_empty(), "fresh vault: snapshot is empty");

    // Alice appends three entries, with one consecutive duplicate
    // that the server-side store should collapse.
    for entry in ["one", "two", "two", "three"] {
        send_frame(
            &mut alice,
            &Frame::HistoryAppend {
                session_id: "sess-alice".into(),
                entry: entry.into(),
            },
        )
        .await
        .expect("alice append");
    }

    // Bob connects on a fresh session. The gateway is the single
    // writer of the vault key, so Bob's snapshot must contain
    // Alice's entries (consecutive duplicate collapsed).
    let bob_snapshot = wait_for_snapshot(port, &tui_token, "bob-", &["one", "two", "three"])
        .await
        .expect("bob snapshot reflects alice's appends");
    assert_eq!(bob_snapshot, vec!["one", "two", "three"]);

    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
}

/// Repeatedly open a TUI connection and return the first snapshot
/// that matches `expected`. Appends over the WS are fire-and-forget
/// so a new client may observe the store before the gateway has
/// persisted the latest append — polling is the simplest way to
/// stay deterministic without reaching into the store directly.
/// Each attempt uses a fresh session id so the registry's
/// per-session guard doesn't reject the re-register while the
/// previous connection is still being torn down.
async fn wait_for_snapshot(
    port: u16,
    tui_token: &str,
    session_prefix: &str,
    expected: &[&str],
) -> Option<Vec<String>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut attempt = 0u32;
    while tokio::time::Instant::now() < deadline {
        let sid = format!("{session_prefix}{attempt}");
        attempt += 1;
        if let Ok((_ws, snapshot)) = connect_register_tui_with_snapshot(port, tui_token, &sid).await
            && snapshot.len() == expected.len()
            && snapshot.iter().zip(expected).all(|(a, b)| a == b)
        {
            return Some(snapshot);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    None
}

async fn connect_register_tui(
    port: u16,
    tui_token: &str,
    session_id: &str,
) -> Result<WsStream, ConnectError> {
    let (ws, _) = connect_register_tui_with_snapshot(port, tui_token, session_id).await?;
    Ok(ws)
}

/// Same as [`connect_register_tui`] but also returns the history
/// snapshot the server pushes right after `RegisterAck`. Used by
/// the history round-trip tests; the regular session-routing tests
/// ignore the snapshot.
async fn connect_register_tui_with_snapshot(
    port: u16,
    tui_token: &str,
    session_id: &str,
) -> Result<(WsStream, Vec<String>), ConnectError> {
    let stream = TcpStream::connect(loopback(port))
        .await
        .map_err(ConnectError::Dial)?;
    let mut request = handshake_url(port)
        .into_client_request()
        .map_err(|e| ConnectError::Upgrade(e.to_string()))?;
    request.headers_mut().insert(
        CHANNEL_TOKEN_HEADER,
        tui_token
            .parse()
            .map_err(|_| ConnectError::Upgrade("invalid tui-token header".into()))?,
    );
    let (mut ws, _) = client_async(request, stream)
        .await
        .map_err(|e| ConnectError::Upgrade(e.to_string()))?;

    send_frame(
        &mut ws,
        &Frame::Register {
            token: String::new(),
            channel_type: ChannelType::from(ChannelType::TUI),
            protocol_version: PROTOCOL_VERSION,
            session_id: Some(session_id.to_owned()),
        },
    )
    .await?;

    match recv_frame(&mut ws).await? {
        Frame::RegisterAck { ok: true, .. } => {}
        Frame::RegisterAck { ok: false, reason } => {
            return Err(ConnectError::RegistrationRejected(
                reason.unwrap_or_else(|| "no reason given".to_string()),
            ));
        }
        _ => return Err(ConnectError::ProtocolViolation("expected RegisterAck")),
    }

    // Session-scoped TUI clients receive a HistorySnapshot right
    // after RegisterAck. Drain it so the caller sees a clean stream
    // of agent-output frames afterward.
    match recv_frame(&mut ws).await? {
        Frame::HistorySnapshot {
            session_id: sid,
            entries,
        } => {
            if sid != session_id {
                return Err(ConnectError::ProtocolViolation(
                    "HistorySnapshot session_id mismatch",
                ));
            }
            Ok((ws, entries))
        }
        _ => Err(ConnectError::ProtocolViolation(
            "expected HistorySnapshot after RegisterAck",
        )),
    }
}
