//! Device-token auth on `/v1/channel-ws`, on the real [`ChannelServer`].
//!
//! Closes the integration seam between the pairing handshake (tested in
//! `device_pair`) and the content self-pull (tested app-side in
//! `baybo-mobile-core`): a paired+**approved** device's persisted `auth_token`
//! authenticates the scoped channel surface through the full production
//! middleware stack, while a still-**pending** device is rejected at the same
//! gate. The post-register content fan-out itself is the exact
//! `ChannelType`-agnostic `Subscribed`-channel machinery `channel_ws` exercises
//! over `http`, and the app-side decode is covered by `baybo-mobile-core`.

use baybo_config::ChannelsConfig;
use baybo_gateway::channel::boot;
use baybo_gateway::channel_listener::ChannelServer;
use baybo_gateway::test_support::build_test_deps;
use baybo_store::device::hash_auth_token;
use baybo_store::{DeviceRow, DeviceStatus};
use tokio::net::TcpStream;
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

fn port_file() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let pf = baybo_workspace::WorkspacePaths::new(dir.path().to_path_buf()).channel_port();
    (dir, pf)
}

fn approved_device(token: &str) -> DeviceRow {
    DeviceRow {
        device_id: "dev-1".into(),
        device_pubkey: vec![0u8; 32],
        auth_token_sha256: hash_auth_token(token),
        status: DeviceStatus::Approved,
        rendezvous_id: Some("11111111-2222-4333-8444-555555555555".into()),
        created_at: 1,
        approved_at: Some(2),
        last_seen_at: None,
        relay_url: "wss://relay.test".into(),
        remote_api_key: "inst-test".into(),
    }
}

/// Attempt the channel-WS upgrade with `token`; returns whether it was accepted
/// (a `101` — i.e. the token authenticated through the full middleware stack).
async fn upgrade_accepted(port: u16, token: &str) -> bool {
    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let req = format!("ws://127.0.0.1:{port}/v1/channel-ws?token={token}")
        .into_client_request()
        .unwrap();
    client_async(req, stream).await.is_ok()
}

/// The security-critical half of the pair→pull chain: an **approved** device's
/// persisted `auth_token` authenticates the channel-WS upgrade through the full
/// production middleware stack (token-table miss → approved-device lookup →
/// `AuthedClient::Device`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approved_device_authenticates_channel_ws_upgrade() {
    let (_dir, pf) = port_file();
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    boot::install_channels(&tg.deps.channel_registry, &ChannelsConfig::default()).expect("install");
    tg.deps
        .stores
        .device
        .create(&approved_device("device-auth-token"))
        .await
        .unwrap();

    let shutdown = tg.shutdown.clone();
    let server = ChannelServer::bind_with_device_store(
        &tg.deps,
        pf,
        tg.channel_tokens.clone(),
        Some(tg.deps.stores.device.clone()),
    )
    .expect("bind");
    let port = server.port();
    let server_shutdown = shutdown.clone();
    let handle = tokio::spawn(async move {
        let _ = server.run(server_shutdown).await;
    });

    assert!(
        upgrade_accepted(port, "device-auth-token").await,
        "an approved device token must authenticate the channel WS upgrade",
    );

    shutdown.trigger();
    let _ = handle.await;
}

/// A **revoked** device is rejected at the same gate — its `auth_token` no
/// longer authenticates anything (the only inert state now that pairing lands
/// rows already-approved).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoked_device_token_is_rejected_on_channel_ws() {
    let (_dir, pf) = port_file();
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    boot::install_channels(&tg.deps.channel_registry, &ChannelsConfig::default()).expect("install");
    let mut revoked = approved_device("revoked-token");
    revoked.status = DeviceStatus::Revoked;
    tg.deps.stores.device.create(&revoked).await.unwrap();

    let shutdown = tg.shutdown.clone();
    let server = ChannelServer::bind_with_device_store(
        &tg.deps,
        pf,
        tg.channel_tokens.clone(),
        Some(tg.deps.stores.device.clone()),
    )
    .expect("bind");
    let port = server.port();
    let server_shutdown = shutdown.clone();
    let handle = tokio::spawn(async move {
        let _ = server.run(server_shutdown).await;
    });

    assert!(
        !upgrade_accepted(port, "revoked-token").await,
        "a revoked device token must NOT authenticate the channel WS upgrade",
    );

    shutdown.trigger();
    let _ = handle.await;
}
