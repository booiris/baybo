//! Round-trip integration coverage for the gateway-side WS channel
//! server.
//!
//! Spins a real [`ChannelServer`] against a temp-dir socket, drives the
//! SDK [`Client`] end-to-end — register → sidecar→agent message →
//! agent→sidecar message → duplicate-register rejection → disconnect
//! cleanup.
//!
//! All assertions live in a single `#[tokio::test]` because the SDK
//! reads `AURA_CHANNEL_SOCKET` / `AURA_CHANNEL_TOKEN` from the process
//! environment; splitting into parallel tests would race on the
//! globally-visible env vars.

use std::env;
use std::sync::Arc;
use std::time::Duration;

use aura_channels::sdk::client::{ENV_CHANNEL_SOCKET, ENV_CHANNEL_TOKEN};
use aura_channels::sdk::wire::Message as WireMessage;
use aura_channels::sdk::{Client, SdkError};
use aura_channels::{AgentOutput, OutgoingMessage};
use aura_gateway::test_support::build_test_deps;
use aura_gateway::uds::ChannelServer;
use aura_gateway_auth::ClientIdentity;
use aura_model::{ChannelType, ContentBlock, MessageMetadata};

const TEST_PSK: [u8; 32] = [0x42; 32];

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

    // SAFETY: `env::set_var` is unsafe in Rust 2024. This integration
    // test binary owns `AURA_CHANNEL_*` exclusively; no other test in
    // the file touches the same vars, and the single-test layout avoids
    // cross-test races on process env.
    unsafe {
        env::set_var(ENV_CHANNEL_SOCKET, &socket_path);
        env::set_var(ENV_CHANNEL_TOKEN, &token);
    }

    let slack = ChannelType::from("slack");

    // 1. Sidecar registers.
    let client = Client::connect(slack.clone())
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
    client.send(outbound.clone()).await.expect("sidecar send");

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

    let recv = tokio::time::timeout(Duration::from_secs(2), client.recv())
        .await
        .expect("sidecar recv timeout")
        .expect("sidecar recv");
    assert_eq!(recv.content, "pong");
    assert_eq!(recv.session_id, "sess-1");
    assert_eq!(recv.channel_type, slack);

    // 4. Duplicate registration for the same channel type is rejected.
    match Client::connect(slack.clone()).await {
        Ok(_) => panic!("duplicate register unexpectedly succeeded"),
        Err(SdkError::RegistrationRejected(msg)) => {
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
