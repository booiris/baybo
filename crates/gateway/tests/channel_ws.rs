//! End-to-end coverage for the channel WS protocol over a real
//! TCP socket.
//!
//! Covers paths the unit tests can't: HTTP upgrade → channel auth
//! middleware → handshake validator → channel attach → Subscribe →
//! agent-side dispatch fan-out.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use baybo_channels::wire::{self, Frame, Message as WireMessage, SessionPatch};
use baybo_channels::{
    AgentEvent, AgentOutput, ChannelKind, MessageRole, OutgoingMessage, RouterInbound,
};
use baybo_config::ChannelsConfig;
use baybo_gateway::auth::{DEVICE_ID_HEADER, WEB_OPERATOR_USER_ID};
use baybo_gateway::channel::boot;
use baybo_gateway::server::{GatewayDeps, build_admin_router_for_tests};
use baybo_gateway::test_support::build_test_deps;
use baybo_model::{ChannelType, ChatMessage, ContentBlock, MessageMetadata, User};
use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

async fn start_admin_ws_server(
    deps: &GatewayDeps,
    shutdown: baybo_agent::service::ShutdownSignal,
) -> Result<(u16, tokio::task::JoinHandle<()>), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let router = build_admin_router_for_tests(deps);
    let handle = tokio::spawn(async move {
        let shutdown_fut = async move {
            shutdown.wait().await;
        };
        let _ = axum::serve(listener, router.into_make_service())
            .with_graceful_shutdown(shutdown_fut)
            .await;
    });
    Ok((port, handle))
}

async fn connect_register(
    port: u16,
    token: &str,
    channel_type: ChannelType,
) -> Result<tokio_tungstenite::WebSocketStream<TcpStream>, Box<dyn std::error::Error + Send + Sync>>
{
    connect_register_with_device_header(port, token, channel_type, None).await
}

async fn connect_register_with_device_header(
    port: u16,
    token: &str,
    channel_type: ChannelType,
    device_id: Option<&str>,
) -> Result<tokio_tungstenite::WebSocketStream<TcpStream>, Box<dyn std::error::Error + Send + Sync>>
{
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let stream = TcpStream::connect(addr).await?;
    let url = format!("ws://127.0.0.1:{port}/v1/channel-ws?token={token}");
    let mut request = url.into_client_request()?;
    if let Some(device_id) = device_id {
        request
            .headers_mut()
            .insert(DEVICE_ID_HEADER, HeaderValue::from_str(device_id)?);
    }
    let (mut ws, _) = client_async(request, stream).await?;

    let frame = Frame::Register {
        token: String::new(),
        channel_type,
    };
    ws.send(WsMessage::Binary(wire::encode(&frame)?)).await?;

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
/// expected content frame in tests that don't care about the pulse —
/// and `Frame::TurnState`, the per-Subscribe in-flight-turn snapshot
/// (asserted directly via [`recv_frame`] by the tests that care).
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
        if !matches!(
            frame,
            Frame::SessionActivity { .. } | Frame::TurnState { .. }
        ) {
            return Ok(frame);
        }
    }
}

async fn send_frame(
    ws: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
    frame: Frame,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    ws.send(WsMessage::Binary(wire::encode(&frame)?)).await?;
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

/// Consume the `TurnState` snapshot the gateway sends to a connection
/// for every `Subscribe` (after the pending-approvals snapshot and any
/// TaskList hydration). The sessions in these tests are idle unless the
/// test says otherwise, so the snapshot is a definitive `active: false`
/// — noise that tests doing raw `recv_frame` assertions next need to
/// step past ([`recv_frame_skip_activity`] skips it automatically).
async fn expect_idle_turn_state(
    ws: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
    expected_session: &str,
) {
    let frame = recv_frame(ws, Duration::from_secs(1))
        .await
        .expect("TurnState snapshot after Subscribe");
    match frame {
        Frame::TurnState {
            session_id,
            active,
            started_at,
            ..
        } => {
            assert_eq!(session_id.as_str(), expected_session);
            assert!(!active, "test sessions are idle; got an active turn");
            assert_eq!(started_at, None);
        }
        other => panic!("expected TurnState, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_token_attaches_web_chat_and_receives_dispatch() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;

    // Eagerly install the http channel — production wires this from
    // ChannelsConfig at boot via `baybo_gateway::channel::boot`.
    let cfg = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &cfg).expect("install");

    let channel_registry = Arc::clone(&tg.deps.channel_registry);
    let shutdown = tg.shutdown.clone();
    let (port, server_handle) = start_admin_ws_server(&tg.deps, shutdown.clone())
        .await
        .expect("bind admin server");

    let mut client = connect_register(port, &tg.deps.admin_token, ChannelType::http())
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
    assert!(http_channel.has_subscribers(&baybo_model::SessionId::from("sess-1")));

    // Server-side dispatch reaches the subscribed client.
    let outgoing = OutgoingMessage {
        session_id: "sess-1".into(),
        user_id: WEB_OPERATOR_USER_ID.into(),
        channel: ChannelType::http(),
        content: vec![ContentBlock::Text("hello".into())],
        reply_to: None,
        metadata: MessageMetadata::default(),
        ordinal: Some(7),
    };
    http_channel.dispatch_agent(outgoing.into());

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
async fn admin_token_with_device_header_registers_device_channel() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let cfg = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &cfg).expect("install");

    let shutdown = tg.shutdown.clone();
    let (port, server_handle) = start_admin_ws_server(&tg.deps, shutdown.clone())
        .await
        .expect("bind admin server");

    let device_key = device_proto::delegation::generate_signing_key();
    let device_id = device_proto::delegation::device_id_for(&device_key.verifying_key());
    let mut client = connect_register_with_device_header(
        port,
        &tg.deps.admin_token,
        ChannelType::device(),
        Some(&device_id),
    )
    .await
    .expect("device WS handshake");

    let device_channel = tg
        .deps
        .channel_registry
        .get(&ChannelType::device())
        .expect("device channel installed");
    assert_eq!(device_channel.kind(), ChannelKind::Subscribed);
    assert_eq!(device_channel.connection_count(), 1);

    let rejected = connect_register_with_device_header(
        port,
        &tg.deps.admin_token,
        ChannelType::http(),
        Some(&device_id),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        rejected.contains("device token must register as channel_type 'device'"),
        "device identity must not be allowed to claim http: {rejected}",
    );

    client.close(None).await.expect("close client");
    shutdown.trigger();
    let _ = server_handle.await;
}

/// A reconnecting / freshly-opening client recovers the durable planning
/// checklist: the gateway ships a `TaskList` snapshot right after `Subscribe`
/// (without waiting for an agent turn), so a reload / WS reset / cache eviction
/// re-hydrates the list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_hydrates_durable_task_list_snapshot() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let cfg = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &cfg).expect("install");

    // Seed a session with one durable task.
    let user = User {
        id: WEB_OPERATOR_USER_ID.into(),
        name: None,
        channel: ChannelType::http(),
    };
    let session = tg
        .deps
        .session_manager
        .create_session(user, ChannelType::http())
        .await
        .expect("create session");
    let now = chrono::Utc::now();
    let task = baybo_model::Task {
        id: baybo_model::TaskId::new(),
        subject: "write the table".into(),
        description: "create session_tasks".into(),
        status: baybo_model::TaskStatus::InProgress,
        depends_on: Vec::new(),
        created_at: now,
        updated_at: now,
    };
    tg.deps
        .stores
        .task
        .create(&session.id, &task)
        .await
        .expect("seed task");

    let shutdown = tg.shutdown.clone();
    let (port, server_handle) = start_admin_ws_server(&tg.deps, shutdown.clone())
        .await
        .expect("bind admin server");

    let mut client = connect_register(port, &tg.deps.admin_token, ChannelType::http())
        .await
        .expect("WS handshake");

    send_frame(
        &mut client,
        Frame::Subscribe {
            session_id: session.id.clone(),
            since_ordinal: None,
        },
    )
    .await
    .expect("send Subscribe");

    // After the empty pending snapshot, the gateway hydrates the checklist.
    expect_empty_pending_snapshot(&mut client, session.id.as_str()).await;
    let frame = recv_frame_skip_activity(&mut client, Duration::from_secs(1))
        .await
        .expect("TaskList snapshot after Subscribe");
    match frame {
        Frame::TaskList {
            session_id, tasks, ..
        } => {
            assert_eq!(session_id, session.id);
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0].subject, "write the table");
            assert_eq!(tasks[0].status, "in_progress");
        }
        other => panic!("expected TaskList, got {other:?}"),
    }

    drop(client);
    shutdown.trigger();
    let _ = server_handle.await;
}

/// A late joiner (new tab, reconnect) learns whether a turn is in flight
/// from the `TurnState` snapshot the gateway derives from the job store on
/// every `Subscribe` — `active: false` on an idle session, `active: true`
/// with the start instant while a turn-kind job is non-terminal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_hydrates_turn_state_snapshot() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let cfg = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &cfg).expect("install");

    let user = User {
        id: WEB_OPERATOR_USER_ID.into(),
        name: None,
        channel: ChannelType::http(),
    };
    let session = tg
        .deps
        .session_manager
        .create_session(user, ChannelType::http())
        .await
        .expect("create session");

    let shutdown = tg.shutdown.clone();
    let (port, server_handle) = start_admin_ws_server(&tg.deps, shutdown.clone())
        .await
        .expect("bind admin server");

    let expect_turn_state = |frame: Frame, want_active: bool| match frame {
        Frame::TurnState {
            session_id,
            active,
            started_at,
            ..
        } => {
            assert_eq!(session_id, session.id);
            assert_eq!(active, want_active);
            assert_eq!(started_at.is_some(), want_active, "started_at iff active");
        }
        other => panic!("expected TurnState, got {other:?}"),
    };

    // Idle session: the snapshot is a definitive `active: false`.
    let mut tab_a = connect_register(port, &tg.deps.admin_token, ChannelType::http())
        .await
        .expect("WS handshake");
    send_frame(
        &mut tab_a,
        Frame::Subscribe {
            session_id: session.id.clone(),
            since_ordinal: None,
        },
    )
    .await
    .expect("send Subscribe");
    expect_empty_pending_snapshot(&mut tab_a, session.id.as_str()).await;
    // No tasks were seeded, so the next frame after the pending snapshot
    // is the TurnState snapshot itself.
    let frame = recv_frame(&mut tab_a, Duration::from_secs(1))
        .await
        .expect("TurnState snapshot after Subscribe");
    expect_turn_state(frame, false);

    // Turn in flight (a non-terminal UserChat job): a fresh tab's
    // snapshot reports it active, with the start instant.
    let job = tg
        .deps
        .job_lifecycle
        .start_job(
            session.id.clone(),
            baybo_model::TriggerKind::User,
            baybo_job::JobShape::Turn,
            baybo_job::JobInput::UserChat { content: vec![] },
            None,
        )
        .await
        .expect("start turn job");
    tg.deps
        .job_lifecycle
        .start(&job.id)
        .await
        .expect("job → InProgress");

    let mut tab_b = connect_register(port, &tg.deps.admin_token, ChannelType::http())
        .await
        .expect("WS handshake (tab b)");
    send_frame(
        &mut tab_b,
        Frame::Subscribe {
            session_id: session.id.clone(),
            since_ordinal: None,
        },
    )
    .await
    .expect("send Subscribe (tab b)");
    expect_empty_pending_snapshot(&mut tab_b, session.id.as_str()).await;
    let frame = recv_frame(&mut tab_b, Duration::from_secs(1))
        .await
        .expect("TurnState snapshot after Subscribe (tab b)");
    expect_turn_state(frame, true);

    drop(tab_a);
    drop(tab_b);
    shutdown.trigger();
    let _ = server_handle.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_subscribers_to_same_session_both_receive_dispatch() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let cfg = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &cfg).expect("install");

    let channel_registry = Arc::clone(&tg.deps.channel_registry);
    let shutdown = tg.shutdown.clone();
    let (port, server_handle) = start_admin_ws_server(&tg.deps, shutdown.clone())
        .await
        .expect("bind admin server");

    let mut tab_a = connect_register(port, &tg.deps.admin_token, ChannelType::http())
        .await
        .expect("tab A handshake");
    let mut tab_b = connect_register(port, &tg.deps.admin_token, ChannelType::http())
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

    http_channel.dispatch_agent(AgentOutput {
        session_id: "shared".into(),
        user_id: WEB_OPERATOR_USER_ID.into(),
        channel: ChannelType::http(),
        event: AgentEvent::AnswerDelta("stream chunk".into()),
    });

    let a = recv_frame_skip_activity(&mut tab_a, Duration::from_secs(2))
        .await
        .expect("tab A received");
    let b = recv_frame_skip_activity(&mut tab_b, Duration::from_secs(2))
        .await
        .expect("tab B received");
    for (label, frame) in [("A", a), ("B", b)] {
        match frame {
            Frame::AnswerDelta {
                session_id, text, ..
            } => {
                assert_eq!(session_id, "shared", "tab {label} session id");
                assert_eq!(text, "stream chunk", "tab {label} text");
            }
            other => panic!("tab {label} expected AnswerDelta, got {other:?}"),
        }
    }

    drop(tab_a);
    drop(tab_b);
    shutdown.trigger();
    let _ = server_handle.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsubscribed_session_does_not_receive_dispatch() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let cfg = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &cfg).expect("install");

    let channel_registry = Arc::clone(&tg.deps.channel_registry);
    let shutdown = tg.shutdown.clone();
    let (port, server_handle) = start_admin_ws_server(&tg.deps, shutdown.clone())
        .await
        .expect("bind admin server");

    let mut client = connect_register(port, &tg.deps.admin_token, ChannelType::http())
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
    expect_idle_turn_state(&mut client, "interesting").await;

    let http_channel = channel_registry.get(&ChannelType::http()).expect("http");
    // Dispatch a content frame (Notice) to an unrelated session. The
    // session-scoped fan-out drops it for this connection because it
    // isn't subscribed. Separately, the http channel's activity
    // observer fires a `SessionActivity` broadcast to every
    // connection regardless of subscription — that's the deliberate
    // sidebar signal — so we should receive exactly the pulse and
    // *nothing else*.
    http_channel.dispatch_agent(AgentOutput {
        session_id: "unrelated".into(),
        user_id: String::new(),
        channel: ChannelType::http(),
        event: AgentEvent::Notice {
            level: baybo_channels::NoticeLevel::Info,
            text: "for some other tab".into(),
        },
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
                matches!(source, baybo_channels::wire::ActivityKind::Assistant),
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
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let cfg = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &cfg).expect("install");

    let channel_registry = Arc::clone(&tg.deps.channel_registry);
    let shutdown = tg.shutdown.clone();
    let (port, server_handle) = start_admin_ws_server(&tg.deps, shutdown.clone())
        .await
        .expect("bind admin server");

    // Two tabs, two personas:
    //   * subscriber — attached and subscribed to "sess-x"
    //   * bystander  — attached but never subscribed
    let mut subscriber = connect_register(port, &tg.deps.admin_token, ChannelType::http())
        .await
        .expect("subscriber handshake");
    let mut bystander = connect_register(port, &tg.deps.admin_token, ChannelType::http())
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
    expect_idle_turn_state(&mut subscriber, "sess-x").await;

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
                pinned: Some(false),
                folder_id: None,
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
    // observer: a UserEcho produces `ActivityKind::User`; a completed
    // agent `Message` produces `ActivityKind::Assistant` (mid-turn
    // streaming events like AnswerDelta deliberately don't pulse).
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let cfg = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &cfg).expect("install");

    let channel_registry = Arc::clone(&tg.deps.channel_registry);
    let shutdown = tg.shutdown.clone();
    let (port, server_handle) = start_admin_ws_server(&tg.deps, shutdown.clone())
        .await
        .expect("bind admin server");

    let mut client = connect_register(port, &tg.deps.admin_token, ChannelType::http())
        .await
        .expect("handshake");

    let http_channel = channel_registry.get(&ChannelType::http()).expect("http");

    // Assistant-side: dispatch the turn's terminal Message for a session the
    // client never subscribed to. The content frame drops on the floor for
    // this connection; the activity pulse broadcasts to every http tab. (A
    // mid-turn AnswerDelta would NOT pulse — only a completed emission does.)
    http_channel.dispatch_agent(
        OutgoingMessage {
            session_id: "sess-bg".into(),
            user_id: String::new(),
            channel: ChannelType::http(),
            content: vec![ContentBlock::Text("agent reply".into())],
            reply_to: None,
            metadata: MessageMetadata::default(),
            ordinal: None,
        }
        .into(),
    );
    let activity = recv_frame(&mut client, Duration::from_secs(1))
        .await
        .expect("assistant activity pulse");
    match activity {
        Frame::SessionActivity {
            session_id, source, ..
        } => {
            assert_eq!(session_id.as_str(), "sess-bg", "assistant pulse session id");
            assert!(
                matches!(source, baybo_channels::wire::ActivityKind::Assistant),
                "expected Assistant source, got {source:?}",
            );
        }
        other => panic!("expected SessionActivity, got {other:?}"),
    }

    // User-side: a UserEcho also runs through the same observer. Drive
    // it via `SubscribedView::echo_inbound`, which dispatches a
    // `SessionEvent::UserEcho` — exactly the path the WS receive loop
    // takes when the agent router forwards an inbound `Frame::Message`.
    let incoming = baybo_channels::IncomingMessage {
        message: baybo_channels::Message {
            id: "msg-1".into(),
            session_id: "sess-bg".into(),
            channel: ChannelType::http(),
            sender: baybo_model::User {
                id: WEB_OPERATOR_USER_ID.into(),
                name: None,
                channel: ChannelType::http(),
            },
            content: vec![baybo_model::ContentBlock::Text("user typed".into())],
            timestamp: chrono::Utc::now(),
            reply_to: None,
            metadata: baybo_model::MessageMetadata::default(),
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
                matches!(source, baybo_channels::wire::ActivityKind::User),
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
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let cfg = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &cfg).expect("install");

    let shutdown = tg.shutdown.clone();
    let mut incoming_rx = tg.incoming_rx;
    let (port, server_handle) = start_admin_ws_server(&tg.deps, shutdown.clone())
        .await
        .expect("bind admin server");

    let mut client = connect_register(port, &tg.deps.admin_token, ChannelType::http())
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
        let inbound = tokio::time::timeout(Duration::from_secs(2), incoming_rx.recv())
            .await
            .expect("router intake delivery timeout")
            .expect("router intake channel closed");
        let RouterInbound::One(msg) = inbound else {
            panic!("expected a single inbound message, got a batch");
        };
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
async fn messages_batch_reaches_router_as_one_ordered_intake() {
    // The web "send every queued message at once" path: a single
    // `Frame::Messages` carrying N user rows must reach the router as ONE
    // `RouterInbound::Batch` (so the actor coalesces them into a single turn),
    // preserving order — never as N separate intakes that could race the
    // actor's coalescing window.
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let cfg = ChannelsConfig::default();
    boot::install_channels(&tg.deps.channel_registry, &cfg).expect("install");

    let shutdown = tg.shutdown.clone();
    let mut incoming_rx = tg.incoming_rx;
    let (port, server_handle) = start_admin_ws_server(&tg.deps, shutdown.clone())
        .await
        .expect("bind admin server");

    let mut client = connect_register(port, &tg.deps.admin_token, ChannelType::http())
        .await
        .expect("handshake");
    send_frame(
        &mut client,
        Frame::Subscribe {
            session_id: "sess-batch".into(),
            since_ordinal: None,
        },
    )
    .await
    .expect("send Subscribe");
    expect_empty_pending_snapshot(&mut client, "sess-batch").await;

    let msg = |id: &str, content: &str| WireMessage {
        content: content.into(),
        session_id: "sess-batch".into(),
        user_id: WEB_OPERATOR_USER_ID.into(),
        channel_type: ChannelType::http(),
        bot_id: String::new(),
        attachments: Vec::new(),
        platform_msg_id: id.into(),
        role: MessageRole::User,
        ordinal: None,
    };
    send_frame(
        &mut client,
        Frame::Messages {
            messages: vec![msg("a", "alpha"), msg("b", "beta"), msg("c", "gamma")],
        },
    )
    .await
    .expect("send batch");

    let inbound = tokio::time::timeout(Duration::from_secs(2), incoming_rx.recv())
        .await
        .expect("router intake delivery timeout")
        .expect("router intake channel closed");
    let RouterInbound::Batch(batch) = inbound else {
        panic!("expected ONE RouterInbound::Batch, got a single message");
    };
    let bodies: Vec<String> = batch
        .iter()
        .map(|incoming| {
            incoming
                .message
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .collect();
    assert_eq!(
        bodies,
        vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()],
        "the batch must reach the router as one ordered group",
    );

    // The batch is a SINGLE intake — no second item follows it.
    let extra = tokio::time::timeout(Duration::from_millis(200), incoming_rx.recv()).await;
    assert!(
        extra.is_err(),
        "a batch must produce exactly one intake, not per-message ones: {extra:?}",
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
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let cfg = ChannelsConfig::default();
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
        ChatMessage::user(vec![ContentBlock::Text("hi".into())])
            .with_platform_msg_id("device-msg-0"),
        // Agent-injected user-role reminder — must NOT replay.
        ChatMessage::agent_context(vec![ContentBlock::Text("[skill reminder]".into())]),
        ChatMessage::assistant(vec![ContentBlock::Text("hello there".into())]),
        ChatMessage::user(vec![ContentBlock::Text("how are you".into())])
            .with_platform_msg_id("device-msg-3"),
        // Tool-result row — must NOT replay.
        ChatMessage::tool_result("t1".into(), "ok".into()),
        ChatMessage::assistant(vec![ContentBlock::Text("doing well".into())]),
    ];
    for msg in rows {
        session_manager
            .append_session_message(&session.id, msg)
            .await
            .expect("append");
    }

    let shutdown = tg.shutdown.clone();
    let (port, server_handle) = start_admin_ws_server(&tg.deps, shutdown.clone())
        .await
        .expect("bind admin server");

    let mut client = connect_register(port, &tg.deps.admin_token, ChannelType::http())
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
    let mut got: Vec<(i64, String, MessageRole, String)> = Vec::new();
    for _ in 0..2 {
        let frame = recv_frame(&mut client, Duration::from_secs(2))
            .await
            .expect("recv catch-up frame");
        match frame {
            Frame::Message(WireMessage {
                content,
                ordinal,
                role,
                platform_msg_id,
                ..
            }) => {
                got.push((
                    ordinal.expect("catch-up frames carry ordinal"),
                    content,
                    role,
                    platform_msg_id,
                ));
            }
            other => panic!("expected Message, got {other:?}"),
        }
    }
    assert_eq!(
        got,
        vec![
            (
                3,
                "how are you".to_string(),
                MessageRole::User,
                "device-msg-3".to_string()
            ),
            (
                5,
                "doing well".to_string(),
                MessageRole::Assistant,
                String::new()
            ),
        ],
        "catch-up replays UI-visible rows above cursor in ordinal order",
    );

    drop(client);
    shutdown.trigger();
    let _ = server_handle.await;
}

/// `Frame::FetchHistory` is the relay leg's REST-less backfill: a subscribed
/// connection asks for a backward page and the gateway replies, on that
/// connection only, with one `Frame::HistoryPage` of UI-visible bubbles (the same
/// projection `Subscribe` catch-up uses — agent-internal rows filtered out), plus
/// the raw `oldest`/`newest_ordinal` bounds and `has_more`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_history_returns_backward_page_of_visible_rows() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let cfg = ChannelsConfig::default();
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

    // Same mix as the catch-up test: visible bubbles interleaved with
    // agent-internal rows the history page must filter out.
    let rows: &[ChatMessage] = &[
        ChatMessage::user(vec![ContentBlock::Text("hi".into())])
            .with_platform_msg_id("device-msg-0"),
        ChatMessage::agent_context(vec![ContentBlock::Text("[skill reminder]".into())]),
        ChatMessage::assistant(vec![ContentBlock::Text("hello there".into())]),
        ChatMessage::user(vec![ContentBlock::Text("how are you".into())])
            .with_platform_msg_id("device-msg-3"),
        ChatMessage::tool_result("t1".into(), "ok".into()),
        ChatMessage::assistant(vec![ContentBlock::Text("doing well".into())]),
    ];
    for msg in rows {
        session_manager
            .append_session_message(&session.id, msg)
            .await
            .expect("append");
    }

    let shutdown = tg.shutdown.clone();
    let (port, server_handle) = start_admin_ws_server(&tg.deps, shutdown.clone())
        .await
        .expect("bind admin server");

    let mut client = connect_register(port, &tg.deps.admin_token, ChannelType::http())
        .await
        .expect("handshake");

    // FetchHistory requires the connection to be subscribed to the session
    // (parity with the inbound Message path), so Subscribe first (no catch-up).
    send_frame(
        &mut client,
        Frame::Subscribe {
            session_id: session.id.clone(),
            since_ordinal: None,
        },
    )
    .await
    .expect("send Subscribe");
    expect_empty_pending_snapshot(&mut client, session.id.as_str()).await;

    // Request the newest page (before_ordinal = None).
    send_frame(
        &mut client,
        Frame::FetchHistory {
            session_id: session.id.clone(),
            before_ordinal: None,
            limit: Some(50),
        },
    )
    .await
    .expect("send FetchHistory");

    // Skip the post-Subscribe snapshots (TaskList / TurnState) until the page.
    let page = loop {
        let frame = recv_frame(&mut client, Duration::from_secs(2))
            .await
            .expect("recv frame");
        if matches!(frame, Frame::HistoryPage { .. }) {
            break frame;
        }
    };
    let Frame::HistoryPage {
        messages,
        oldest_ordinal,
        newest_ordinal,
        has_more,
        ..
    } = page
    else {
        unreachable!()
    };

    let visible: Vec<(i64, String, MessageRole, String)> = messages
        .into_iter()
        .map(|m| {
            (
                m.ordinal.expect("history rows carry ordinal"),
                m.content,
                m.role,
                m.platform_msg_id,
            )
        })
        .collect();
    assert_eq!(
        visible,
        vec![
            (
                0,
                "hi".to_string(),
                MessageRole::User,
                "device-msg-0".to_string()
            ),
            (
                2,
                "hello there".to_string(),
                MessageRole::Assistant,
                String::new()
            ),
            (
                3,
                "how are you".to_string(),
                MessageRole::User,
                "device-msg-3".to_string()
            ),
            (
                5,
                "doing well".to_string(),
                MessageRole::Assistant,
                String::new()
            ),
        ],
        "history page carries UI-visible bubbles in ascending ordinal order",
    );
    // Raw page bounds span every row (incl. the filtered ones); 6 rows < limit,
    // so nothing older remains.
    assert_eq!(oldest_ordinal, Some(0));
    assert_eq!(newest_ordinal, Some(5));
    assert!(!has_more);

    drop(client);
    shutdown.trigger();
    let _ = server_handle.await;
}
