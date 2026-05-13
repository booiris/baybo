//! End-to-end coverage for the channel WS protocol over a real
//! TCP socket.
//!
//! Covers paths the unit tests can't: HTTP upgrade → channel auth
//! middleware → handshake validator → channel attach → Subscribe →
//! agent-side dispatch fan-out.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use aura_channels::wire::{self, Frame, Message as WireMessage, SessionPatch};
use aura_channels::{AgentOutput, ChannelKind, MessageRole, OutgoingMessage};
use aura_config::ChannelsConfig;
use aura_gateway::auth::{
    ChannelTokenTable, ClientIdentity, TokenHandle, WEB_CLIENT_LABEL_PREFIX, WEB_OPERATOR_USER_ID,
};
use aura_gateway::channel::{StashedTokenHandle, boot};
use aura_gateway::channel_listener::ChannelServer;
use aura_gateway::test_support::build_test_deps;
use aura_model::{ChannelType, ChatMessage, ContentBlock, MessageMetadata, Role, User};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// Mint a web-flavoured channel-token (the exact shape
/// `POST /v1/chat/sessions` mints) and return it together with the
/// owning handle. Caller keeps the handle alive for the test's
/// duration so the live table doesn't revoke the token mid-stream.
fn mint_web_token(tokens: &ChannelTokenTable, slot: &str) -> (String, TokenHandle) {
    let handle = tokens.mint(ClientIdentity {
        pid: std::process::id(),
        label: format!("{WEB_CLIENT_LABEL_PREFIX}{slot}"),
        bound_channel_type: Some(ChannelType::http().to_string()),
    });
    (handle.token().to_string(), handle)
}

async fn connect_register(
    port: u16,
    token: &str,
    channel_type: ChannelType,
) -> Result<tokio_tungstenite::WebSocketStream<TcpStream>, Box<dyn std::error::Error + Send + Sync>>
{
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let stream = TcpStream::connect(addr).await?;
    let url = format!("ws://127.0.0.1:{port}/v1/channel-ws?token={token}");
    let request = url.into_client_request()?;
    let (mut ws, _) = client_async(request, stream).await?;

    let frame = Frame::Register {
        token: String::new(),
        channel_type,
    };
    ws.send(WsMessage::Binary(wire::encode(&frame)?.into()))
        .await?;

    // Drain RegisterAck.
    let next = match tokio::time::timeout(Duration::from_secs(2), ws.next()).await {
        Ok(Some(Ok(msg))) => msg,
        Ok(Some(Err(e))) => return Err(format!("ws read error: {e}").into()),
        Ok(None) => return Err("peer closed before RegisterAck".into()),
        Err(_) => return Err("RegisterAck timeout".into()),
    };
    let ack = match next {
        WsMessage::Binary(bytes) => wire::decode(&bytes)?,
        other => return Err(format!("expected Binary RegisterAck, got {other:?}").into()),
    };
    match ack {
        Frame::RegisterAck { ok: true, .. } => Ok(ws),
        Frame::RegisterAck { ok: false, reason } => {
            Err(format!("RegisterAck rejected: {}", reason.unwrap_or_default()).into())
        }
        other => Err(format!("expected RegisterAck, got {other:?}").into()),
    }
}

async fn recv_frame(
    ws: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
    timeout: Duration,
) -> Result<Frame, Box<dyn std::error::Error + Send + Sync>> {
    let next = match tokio::time::timeout(timeout, ws.next()).await {
        Ok(Some(Ok(msg))) => msg,
        Ok(Some(Err(e))) => return Err(format!("ws read error: {e}").into()),
        Ok(None) => return Err("peer closed".into()),
        Err(_) => return Err("recv timeout".into()),
    };
    match next {
        WsMessage::Binary(bytes) => Ok(wire::decode(&bytes)?),
        other => Err(format!("expected Binary, got {other:?}").into()),
    }
}

/// Like [`recv_frame`] but transparently skips `Frame::SessionActivity`
/// — the sidebar pulse broadcasts unconditionally to every `http`
/// connection on dispatch, which would interpose itself before every
/// expected content frame in tests that don't care about the pulse.
async fn recv_frame_skip_activity(
    ws: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
    timeout: Duration,
) -> Result<Frame, Box<dyn std::error::Error + Send + Sync>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("recv timeout".into());
        }
        let frame = recv_frame(ws, remaining).await?;
        if !matches!(frame, Frame::SessionActivity { .. }) {
            return Ok(frame);
        }
    }
}

async fn send_frame(
    ws: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
    frame: Frame,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    ws.send(WsMessage::Binary(wire::encode(&frame)?.into()))
        .await?;
    Ok(())
}

/// Consume the empty `PendingApprovalsSnapshot` the gateway sends to a
/// connection right after a `Subscribe` is registered. The tests in
/// this file set up sessions with no pre-existing pending approvals,
/// so the snapshot is always empty — it's just noise that the
/// "expect next frame to be X" assertions need to skip past.
async fn expect_empty_pending_snapshot(
    ws: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
    expected_session: &str,
) {
    let frame = recv_frame(ws, Duration::from_secs(1))
        .await
        .expect("PendingApprovalsSnapshot after Subscribe");
    match frame {
        Frame::PendingApprovalsSnapshot {
            session_id,
            call_ids,
        } => {
            assert_eq!(session_id.as_str(), expected_session);
            assert!(
                call_ids.is_empty(),
                "no pending approvals expected in test setup; got {call_ids:?}"
            );
        }
        other => panic!("expected PendingApprovalsSnapshot, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_token_attaches_subscribes_and_receives_dispatch() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let port_file =
        aura_workspace::WorkspacePaths::new(tempdir.path().to_path_buf()).channel_port();

    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;

    // Eagerly install the http channel — production wires this from
    // ChannelsConfig at boot via `aura_gateway::channel::boot`.
    let mut cfg = ChannelsConfig::default();
    cfg.http = Some(aura_config::HttpChannelConfig {
        enabled: true,
        bind_address: "127.0.0.1".into(),
        port: 0,
    });
    boot::install_channels(&tg.deps.channel_registry, &cfg).expect("install");

    let channel_registry = Arc::clone(&tg.deps.channel_registry);
    let channel_tokens = tg.channel_tokens.clone();
    let shutdown = tg.shutdown.clone();
    let server = ChannelServer::bind(&tg.deps, port_file, channel_tokens.clone())
        .expect("bind ChannelServer");
    let port = server.port();

    let server_shutdown = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        let _ = server.run(server_shutdown).await;
    });

    let (token, _handle) = mint_web_token(&channel_tokens, "tab-a");
    let mut client = connect_register(port, &token, ChannelType::http())
        .await
        .expect("WS handshake");

    let http_channel = channel_registry
        .get(&ChannelType::http())
        .expect("http channel installed");
    assert_eq!(http_channel.kind(), ChannelKind::Subscribed);
    // One connection attached after the handshake.
    assert_eq!(http_channel.connection_count(), 1);

    // Subscribe to a session.
    send_frame(
        &mut client,
        Frame::Subscribe {
            session_id: "sess-1".into(),
            since_ordinal: None,
        },
    )
    .await
    .expect("send Subscribe");

    // Drain the snapshot frame the gateway sends right after Subscribe
    // and use it as the "subscribe processed" signal — `has_subscribers`
    // is true as soon as the snapshot lands on the wire.
    expect_empty_pending_snapshot(&mut client, "sess-1").await;
    assert!(http_channel.has_subscribers(&aura_model::SessionId::from("sess-1")));

    // Server-side dispatch reaches the subscribed client.
    let outgoing = OutgoingMessage {
        session_id: "sess-1".into(),
        user_id: WEB_OPERATOR_USER_ID.into(),
        channel: ChannelType::http(),
        content: vec![ContentBlock::Text("hello".into())],
        reply_to: None,
        metadata: MessageMetadata::default(),
    };
    http_channel.dispatch_agent(AgentOutput::Message(outgoing));

    let frame = recv_frame_skip_activity(&mut client, Duration::from_secs(2))
        .await
        .expect("client received dispatched message");
    match frame {
        Frame::Message(WireMessage {
            content,
            session_id,
            role,
            ..
        }) => {
            assert_eq!(content, "hello");
            assert_eq!(session_id, "sess-1");
            assert!(matches!(role, MessageRole::Assistant));
        }
        other => panic!("expected Message frame, got {other:?}"),
    }

    drop(client);
    shutdown.trigger();
    let _ = server_handle.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_subscribers_to_same_session_both_receive_dispatch() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let port_file =
        aura_workspace::WorkspacePaths::new(tempdir.path().to_path_buf()).channel_port();

    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let mut cfg = ChannelsConfig::default();
    cfg.http = Some(aura_config::HttpChannelConfig {
        enabled: true,
        bind_address: "127.0.0.1".into(),
        port: 0,
    });
    boot::install_channels(&tg.deps.channel_registry, &cfg).expect("install");

    let channel_registry = Arc::clone(&tg.deps.channel_registry);
    let channel_tokens = tg.channel_tokens.clone();
    let shutdown = tg.shutdown.clone();
    let server = ChannelServer::bind(&tg.deps, port_file, channel_tokens.clone())
        .expect("bind ChannelServer");
    let port = server.port();

    let server_shutdown = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        let _ = server.run(server_shutdown).await;
    });

    let (token_a, _ha) = mint_web_token(&channel_tokens, "tab-a");
    let (token_b, _hb) = mint_web_token(&channel_tokens, "tab-b");
    let mut tab_a = connect_register(port, &token_a, ChannelType::http())
        .await
        .expect("tab A handshake");
    let mut tab_b = connect_register(port, &token_b, ChannelType::http())
        .await
        .expect("tab B handshake");

    let http_channel = channel_registry.get(&ChannelType::http()).expect("http");
    assert_eq!(http_channel.connection_count(), 2);

    send_frame(
        &mut tab_a,
        Frame::Subscribe {
            session_id: "shared".into(),
            since_ordinal: None,
        },
    )
    .await
    .expect("tab A subscribe");
    send_frame(
        &mut tab_b,
        Frame::Subscribe {
            session_id: "shared".into(),
            since_ordinal: None,
        },
    )
    .await
    .expect("tab B subscribe");
    expect_empty_pending_snapshot(&mut tab_a, "shared").await;
    expect_empty_pending_snapshot(&mut tab_b, "shared").await;

    http_channel.dispatch_agent(AgentOutput::Delta {
        session_id: "shared".into(),
        user_id: WEB_OPERATOR_USER_ID.into(),
        channel: ChannelType::http(),
        text: "stream chunk".into(),
    });

    let a = recv_frame_skip_activity(&mut tab_a, Duration::from_secs(2))
        .await
        .expect("tab A received");
    let b = recv_frame_skip_activity(&mut tab_b, Duration::from_secs(2))
        .await
        .expect("tab B received");
    for (label, frame) in [("A", a), ("B", b)] {
        match frame {
            Frame::Delta {
                session_id, text, ..
            } => {
                assert_eq!(session_id, "shared", "tab {label} session id");
                assert_eq!(text, "stream chunk", "tab {label} text");
            }
            other => panic!("tab {label} expected Delta, got {other:?}"),
        }
    }

    drop(tab_a);
    drop(tab_b);
    shutdown.trigger();
    let _ = server_handle.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsubscribed_session_does_not_receive_dispatch() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let port_file =
        aura_workspace::WorkspacePaths::new(tempdir.path().to_path_buf()).channel_port();

    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let mut cfg = ChannelsConfig::default();
    cfg.http = Some(aura_config::HttpChannelConfig {
        enabled: true,
        bind_address: "127.0.0.1".into(),
        port: 0,
    });
    boot::install_channels(&tg.deps.channel_registry, &cfg).expect("install");

    let channel_registry = Arc::clone(&tg.deps.channel_registry);
    let channel_tokens = tg.channel_tokens.clone();
    let shutdown = tg.shutdown.clone();
    let server = ChannelServer::bind(&tg.deps, port_file, channel_tokens.clone())
        .expect("bind ChannelServer");
    let port = server.port();

    let server_shutdown = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        let _ = server.run(server_shutdown).await;
    });

    let (token, _handle) = mint_web_token(&channel_tokens, "watch");
    let mut client = connect_register(port, &token, ChannelType::http())
        .await
        .expect("handshake");
    send_frame(
        &mut client,
        Frame::Subscribe {
            session_id: "interesting".into(),
            since_ordinal: None,
        },
    )
    .await
    .expect("subscribe");
    expect_empty_pending_snapshot(&mut client, "interesting").await;

    let http_channel = channel_registry.get(&ChannelType::http()).expect("http");
    // Dispatch a content frame (Notice) to an unrelated session. The
    // session-scoped fan-out drops it for this connection because it
    // isn't subscribed. Separately, the http channel's activity
    // observer fires a `SessionActivity` broadcast to every
    // connection regardless of subscription — that's the deliberate
    // sidebar signal — so we should receive exactly the pulse and
    // *nothing else*.
    http_channel.dispatch_agent(AgentOutput::Notice {
        session_id: "unrelated".into(),
        user_id: String::new(),
        channel: ChannelType::http(),
        level: aura_channels::NoticeLevel::Info,
        text: "for some other tab".into(),
    });

    let activity = recv_frame(&mut client, Duration::from_secs(1))
        .await
        .expect("sidebar activity pulse");
    match activity {
        Frame::SessionActivity {
            session_id, source, ..
        } => {
            assert_eq!(session_id.as_str(), "unrelated", "activity session id");
            assert!(
                matches!(source, aura_channels::wire::ActivityKind::Assistant),
                "activity source: {source:?}",
            );
        }
        other => panic!("expected SessionActivity pulse, got {other:?}"),
    }
    // Now confirm the actual Notice content frame doesn't follow —
    // subscription-gated fan-out filters it for this connection.
    let result = tokio::time::timeout(Duration::from_millis(200), client.next()).await;
    assert!(
        result.is_err(),
        "subscription-gated content frame should not reach unsubscribed connection"
    );

    drop(client);
    shutdown.trigger();
    let _ = server_handle.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_list_broadcast_reaches_every_web_tab() {
    // Web sidebar sync model: when any tab creates / hides / unhides a
    // chat session (or its `last_active` bumps), the gateway pushes
    // `Frame::SessionUpdated` with a sparse `SessionPatch` to every
    // connection on the `http` channel — including connections
    // subscribed to a *different* session, and connections that haven't
    // subscribed to any session at all. This is the contract the
    // sidebar relies on to converge across browsers / devices without
    // polling.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let port_file =
        aura_workspace::WorkspacePaths::new(tempdir.path().to_path_buf()).channel_port();

    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let mut cfg = ChannelsConfig::default();
    cfg.http = Some(aura_config::HttpChannelConfig {
        enabled: true,
        bind_address: "127.0.0.1".into(),
        port: 0,
    });
    boot::install_channels(&tg.deps.channel_registry, &cfg).expect("install");

    let channel_registry = Arc::clone(&tg.deps.channel_registry);
    let channel_tokens = tg.channel_tokens.clone();
    let shutdown = tg.shutdown.clone();
    let server = ChannelServer::bind(&tg.deps, port_file, channel_tokens.clone())
        .expect("bind ChannelServer");
    let port = server.port();
    let server_shutdown = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        let _ = server.run(server_shutdown).await;
    });

    // Two tabs, two personas:
    //   * subscriber — attached and subscribed to "sess-x"
    //   * bystander  — attached but never subscribed
    let (tok_sub, _h_sub) = mint_web_token(&channel_tokens, "subscriber");
    let (tok_by, _h_by) = mint_web_token(&channel_tokens, "bystander");
    let mut subscriber = connect_register(port, &tok_sub, ChannelType::http())
        .await
        .expect("subscriber handshake");
    let mut bystander = connect_register(port, &tok_by, ChannelType::http())
        .await
        .expect("bystander handshake");

    send_frame(
        &mut subscriber,
        Frame::Subscribe {
            session_id: "sess-x".into(),
            since_ordinal: None,
        },
    )
    .await
    .expect("send Subscribe");
    expect_empty_pending_snapshot(&mut subscriber, "sess-x").await;

    let http_channel = channel_registry.get(&ChannelType::http()).expect("http");
    let created_at = chrono::Utc::now();
    http_channel
        .as_subscribed()
        .expect("http channel is Subscribed")
        .broadcast_session_patch(
            "new-session".into(),
            SessionPatch {
                created_at: Some(created_at),
                last_active: Some(created_at),
                hidden: Some(false),
            },
        );

    for (label, ws) in [
        ("subscriber", &mut subscriber),
        ("bystander", &mut bystander),
    ] {
        let frame = recv_frame(ws, Duration::from_secs(2))
            .await
            .unwrap_or_else(|e| panic!("{label} did not receive frame: {e}"));
        match frame {
            Frame::SessionUpdated { session_id, patch } => {
                assert_eq!(session_id, "new-session", "{label}: session_id");
                assert_eq!(patch.created_at, Some(created_at), "{label}: created_at");
                assert_eq!(patch.last_active, Some(created_at), "{label}: last_active");
                assert_eq!(patch.hidden, Some(false), "{label}: hidden");
            }
            other => panic!("{label}: expected SessionUpdated, got {other:?}"),
        }
    }

    drop(subscriber);
    drop(bystander);
    shutdown.trigger();
    let _ = server_handle.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_activity_pulse_reaches_unsubscribed_tab() {
    // The whole point of `Frame::SessionActivity`: a tab that's not
    // subscribed to session F still gets a cheap unread signal when
    // F sees activity, without paying for F's full content stream.
    // This exercises both directions through the same dispatch
    // observer: a UserEcho should produce `ActivityKind::User`; an
    // agent Delta should produce `ActivityKind::Assistant`.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let port_file =
        aura_workspace::WorkspacePaths::new(tempdir.path().to_path_buf()).channel_port();

    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let mut cfg = ChannelsConfig::default();
    cfg.http = Some(aura_config::HttpChannelConfig {
        enabled: true,
        bind_address: "127.0.0.1".into(),
        port: 0,
    });
    boot::install_channels(&tg.deps.channel_registry, &cfg).expect("install");

    let channel_registry = Arc::clone(&tg.deps.channel_registry);
    let channel_tokens = tg.channel_tokens.clone();
    let shutdown = tg.shutdown.clone();
    let server = ChannelServer::bind(&tg.deps, port_file, channel_tokens.clone())
        .expect("bind ChannelServer");
    let port = server.port();
    let server_shutdown = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        let _ = server.run(server_shutdown).await;
    });

    let (token, _h) = mint_web_token(&channel_tokens, "bystander");
    let mut client = connect_register(port, &token, ChannelType::http())
        .await
        .expect("handshake");

    let http_channel = channel_registry.get(&ChannelType::http()).expect("http");

    // Assistant-side: dispatch a Delta for a session the client never
    // subscribed to. Content frame drops on the floor for this
    // connection; the activity pulse broadcasts to every http tab.
    http_channel.dispatch_agent(AgentOutput::Delta {
        session_id: "sess-bg".into(),
        user_id: String::new(),
        channel: ChannelType::http(),
        text: "agent reply".into(),
    });
    let activity = recv_frame(&mut client, Duration::from_secs(1))
        .await
        .expect("assistant activity pulse");
    match activity {
        Frame::SessionActivity {
            session_id, source, ..
        } => {
            assert_eq!(session_id.as_str(), "sess-bg", "assistant pulse session id");
            assert!(
                matches!(source, aura_channels::wire::ActivityKind::Assistant),
                "expected Assistant source, got {source:?}",
            );
        }
        other => panic!("expected SessionActivity, got {other:?}"),
    }

    // User-side: a UserEcho also runs through the same observer. Drive
    // it via `SubscribedView::echo_inbound`, which dispatches a
    // `SessionEvent::UserEcho` — exactly the path the WS receive loop
    // takes when the agent router forwards an inbound `Frame::Message`.
    let incoming = aura_channels::IncomingMessage {
        message: aura_channels::Message {
            id: "msg-1".into(),
            session_id: "sess-bg".into(),
            channel: ChannelType::http(),
            sender: aura_model::User {
                id: WEB_OPERATOR_USER_ID.into(),
                name: None,
                channel: ChannelType::http(),
            },
            content: vec![aura_model::ContentBlock::Text("user typed".into())],
            timestamp: chrono::Utc::now(),
            reply_to: None,
            metadata: aura_model::MessageMetadata::default(),
        },
        platform_msg_id: String::new(),
    };
    // 1.5s throttle window: wait it out so the second pulse isn't
    // coalesced into the first.
    tokio::time::sleep(Duration::from_millis(1600)).await;
    http_channel
        .as_subscribed()
        .expect("http channel is Subscribed")
        .echo_inbound(incoming);
    let activity = recv_frame(&mut client, Duration::from_secs(1))
        .await
        .expect("user activity pulse");
    match activity {
        Frame::SessionActivity {
            session_id, source, ..
        } => {
            assert_eq!(session_id.as_str(), "sess-bg", "user pulse session id");
            assert!(
                matches!(source, aura_channels::wire::ActivityKind::User),
                "expected User source, got {source:?}",
            );
        }
        other => panic!("expected SessionActivity, got {other:?}"),
    }

    drop(client);
    shutdown.trigger();
    let _ = server_handle.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_platform_msg_id_drops_retry_on_subscribed_channel() {
    // Idempotent Send: a web tab that double-sends — same Send button
    // hit twice in flight, or the WS dropped between send and echo and
    // the user retried — provides the same `platform_msg_id` on every
    // attempt. The gateway's `InboundDedup` rejects every attempt past
    // the first inside its recency window, so the router sees a single
    // inbound and the agent doesn't pay for two turns.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let port_file =
        aura_workspace::WorkspacePaths::new(tempdir.path().to_path_buf()).channel_port();

    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let mut cfg = ChannelsConfig::default();
    cfg.http = Some(aura_config::HttpChannelConfig {
        enabled: true,
        bind_address: "127.0.0.1".into(),
        port: 0,
    });
    boot::install_channels(&tg.deps.channel_registry, &cfg).expect("install");

    let channel_tokens = tg.channel_tokens.clone();
    let shutdown = tg.shutdown.clone();
    let mut incoming_rx = tg.incoming_rx;
    let server =
        ChannelServer::bind(&tg.deps, port_file, channel_tokens.clone()).expect("bind server");
    let port = server.port();
    let server_shutdown = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        let _ = server.run(server_shutdown).await;
    });

    let (token, _handle) = mint_web_token(&channel_tokens, "tab-dedup");
    let mut client = connect_register(port, &token, ChannelType::http())
        .await
        .expect("handshake");
    send_frame(
        &mut client,
        Frame::Subscribe {
            session_id: "sess-dedup".into(),
            since_ordinal: None,
        },
    )
    .await
    .expect("send Subscribe");
    expect_empty_pending_snapshot(&mut client, "sess-dedup").await;

    let send_msg = |id: &str, content: &str| WireMessage {
        content: content.into(),
        session_id: "sess-dedup".into(),
        user_id: WEB_OPERATOR_USER_ID.into(),
        channel_type: ChannelType::http(),
        bot_id: String::new(),
        attachments: Vec::new(),
        platform_msg_id: id.into(),
        role: MessageRole::User,
        ordinal: None,
    };

    send_frame(&mut client, Frame::Message(send_msg("m1", "first")))
        .await
        .expect("send #1");
    send_frame(
        &mut client,
        Frame::Message(send_msg("m1", "retry of first")),
    )
    .await
    .expect("send #2");
    send_frame(&mut client, Frame::Message(send_msg("m2", "third")))
        .await
        .expect("send #3");

    let mut delivered: Vec<String> = Vec::new();
    for _ in 0..2 {
        let msg = tokio::time::timeout(Duration::from_secs(2), incoming_rx.recv())
            .await
            .expect("router intake delivery timeout")
            .expect("router intake channel closed");
        let body = msg
            .message
            .content
            .iter()
            .filter_map(|b| {
                if let ContentBlock::Text(t) = b {
                    Some(t.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("");
        delivered.push(body);
    }
    assert_eq!(
        delivered,
        vec!["first".to_string(), "third".to_string()],
        "dedup must drop the retry; first + third reach the router",
    );

    // The retry frame must not produce a third intake. A short read
    // timeout after the two real deliveries proves nothing is queued
    // behind them.
    let extra = tokio::time::timeout(Duration::from_millis(200), incoming_rx.recv()).await;
    assert!(
        extra.is_err(),
        "duplicate platform_msg_id leaked through to the router: {extra:?}",
    );

    drop(client);
    shutdown.trigger();
    let _ = server_handle.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_with_since_ordinal_replays_missed_messages() {
    // Reconnect cursor model: a client that briefly lost the WS sends
    // `Subscribe { since_ordinal: Some(N) }` on its next attach, and
    // the gateway streams every persisted Message row with ordinal > N
    // to that one connection. Agent-injected rows (skill reminders,
    // tool calls, system frames) are filtered out — the catch-up has
    // to match what a continuously-connected client would have seen.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let port_file =
        aura_workspace::WorkspacePaths::new(tempdir.path().to_path_buf()).channel_port();
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let mut cfg = ChannelsConfig::default();
    cfg.http = Some(aura_config::HttpChannelConfig {
        enabled: true,
        bind_address: "127.0.0.1".into(),
        port: 0,
    });
    boot::install_channels(&tg.deps.channel_registry, &cfg).expect("install");

    let session_manager = Arc::clone(&tg.deps.session_manager);
    let user = User {
        id: WEB_OPERATOR_USER_ID.into(),
        name: None,
        channel: ChannelType::http(),
    };
    let session = session_manager
        .create_session(user, ChannelType::http())
        .await
        .expect("create session");

    // Mix of rows: visible bubbles + agent-internal rows that the
    // catch-up replay must skip.
    let rows: &[ChatMessage] = &[
        ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text("hi".into())],
            from_user: true,
        },
        // Agent-injected user-role reminder — must NOT replay.
        ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text("[skill reminder]".into())],
            from_user: false,
        },
        ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text("hello there".into())],
            from_user: false,
        },
        ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text("how are you".into())],
            from_user: true,
        },
        // Tool-result row — must NOT replay.
        ChatMessage {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "ok".into(),
            }],
            from_user: false,
        },
        ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text("doing well".into())],
            from_user: false,
        },
    ];
    for msg in rows {
        session_manager
            .append_session_message(&session.id, msg)
            .await
            .expect("append");
    }

    let channel_tokens = tg.channel_tokens.clone();
    let shutdown = tg.shutdown.clone();
    let server =
        ChannelServer::bind(&tg.deps, port_file, channel_tokens.clone()).expect("bind server");
    let port = server.port();
    let server_shutdown = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        let _ = server.run(server_shutdown).await;
    });

    let (token, _handle) = mint_web_token(&channel_tokens, "reconnect");
    let mut client = connect_register(port, &token, ChannelType::http())
        .await
        .expect("handshake");

    // Pretend the client only saw the first two visible bubbles (the
    // first User and the first Assistant) — ordinals 0 and 2 in the
    // append order, since the skill-reminder row took ordinal 1. The
    // cursor it sends is the highest one it actually saw: 2.
    send_frame(
        &mut client,
        Frame::Subscribe {
            session_id: session.id.clone(),
            since_ordinal: Some(2),
        },
    )
    .await
    .expect("send Subscribe with cursor");
    expect_empty_pending_snapshot(&mut client, session.id.as_str()).await;

    // Catch-up should stream: row #3 (user "how are you", ord=3) and
    // row #5 (assistant "doing well", ord=5). Row #4 (tool result) is
    // skipped by the visibility filter.
    let mut got: Vec<(i64, String, MessageRole)> = Vec::new();
    for _ in 0..2 {
        let frame = recv_frame(&mut client, Duration::from_secs(2))
            .await
            .expect("recv catch-up frame");
        match frame {
            Frame::Message(WireMessage {
                content,
                ordinal,
                role,
                ..
            }) => {
                got.push((
                    ordinal.expect("catch-up frames carry ordinal"),
                    content,
                    role,
                ));
            }
            other => panic!("expected Message, got {other:?}"),
        }
    }
    assert_eq!(
        got,
        vec![
            (3, "how are you".to_string(), MessageRole::User),
            (5, "doing well".to_string(), MessageRole::Assistant),
        ],
        "catch-up replays UI-visible rows above cursor in ordinal order",
    );

    drop(client);
    shutdown.trigger();
    let _ = server_handle.await;
}

/// The web mint endpoint stashes a `TokenHandle` in
/// `web_chat_tokens`; the channel-WS upgrade is supposed to remove it
/// from that stash and bind the handle to the connection's `Sidecar`,
/// so closing the WS revokes the token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_ws_upgrade_takes_handle_and_revokes_on_close() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let port_file =
        aura_workspace::WorkspacePaths::new(tempdir.path().to_path_buf()).channel_port();

    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let mut cfg = ChannelsConfig::default();
    cfg.http = Some(aura_config::HttpChannelConfig {
        enabled: true,
        bind_address: "127.0.0.1".into(),
        port: 0,
    });
    boot::install_channels(&tg.deps.channel_registry, &cfg).expect("install");

    let channel_tokens = tg.channel_tokens.clone();
    let web_chat_tokens = Arc::clone(&tg.deps.web_chat_tokens);
    let shutdown = tg.shutdown.clone();
    let server =
        ChannelServer::bind(&tg.deps, port_file, channel_tokens.clone()).expect("bind server");
    let port = server.port();
    let server_shutdown = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        let _ = server.run(server_shutdown).await;
    });

    // Mimic the admin mint path: mint a token and stash the handle
    // keyed by token string.
    let handle = channel_tokens.mint(ClientIdentity {
        pid: std::process::id(),
        label: format!("{WEB_CLIENT_LABEL_PREFIX}lifecycle-test"),
        bound_channel_type: Some(ChannelType::http().to_string()),
    });
    let token = handle.token().to_owned();
    web_chat_tokens.insert(token.clone(), StashedTokenHandle::new(handle));
    assert!(
        channel_tokens.lookup(&token).is_some(),
        "fresh mint is live"
    );
    assert!(web_chat_tokens.contains_key(&token), "handle stashed");

    let client = connect_register(port, &token, ChannelType::http())
        .await
        .expect("handshake");

    // After upgrade, the stash entry is gone — the handle has been
    // moved into the Sidecar — but the token itself stays live for
    // the duration of the WS.
    assert!(
        !web_chat_tokens.contains_key(&token),
        "WS upgrade should have removed the stashed handle",
    );
    assert!(
        channel_tokens.lookup(&token).is_some(),
        "token stays live while the WS is open (Sidecar owns the handle)",
    );

    drop(client);

    // Server-side close detection is async — poll until the Sidecar
    // drops its handle (or fail the test after a generous deadline).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while channel_tokens.lookup(&token).is_some() {
        if tokio::time::Instant::now() >= deadline {
            panic!("token should have been revoked after WS close");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    shutdown.trigger();
    let _ = server_handle.await;
}
