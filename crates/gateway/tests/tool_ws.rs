//! End-to-end coverage for the `/v1/tool-ws` route + the
//! `WsBrowserSidecarClient` it backs.
//!
//! Spins a real [`ChannelServer`] on an ephemeral loopback port,
//! plumbs a [`WsBrowserSidecarClient`] into `GatewayDeps`, plays the
//! browser sidecar role from a tokio task (Register handshake, then
//! auto-reply to any incoming `RpcRequest`), and asserts that
//! `BrowserSidecarClient::call` round-trips through the WS hop and
//! returns the sidecar-supplied result.
//!
//! No real Chromium required — the test stops at the protocol layer.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use aura_gateway::channel_listener::ChannelServer;
use aura_gateway::test_support::build_test_deps;
use aura_gateway::tool_ws::WsBrowserSidecarClient;
use aura_gateway::tool_ws::frame::{PROTOCOL_VERSION, ToolFrame, decode, encode};
use aura_gateway::{CHANNEL_TOKEN_HEADER, ChannelTokenTable, ClientIdentity};
use aura_tools::builtin::browser::BrowserSidecarClient;
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::TcpStream;
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_util::sync::CancellationToken;

const TOOL_BROWSER_LABEL: &str = "tool/browser";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_ws_round_trip_through_sidecar_client() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let port_file =
        aura_workspace::WorkspacePaths::new(tempdir.path().to_path_buf()).channel_port();

    // The test-support helper defaults `tool_ws_client: None`. We
    // swap in a real one (and the matching label) so `build_channel_router`
    // mounts `/v1/tool-ws`.
    let mut tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let ws_client = WsBrowserSidecarClient::new();
    tg.deps.tool_ws_client = Some(ws_client.clone());
    tg.deps.tool_ws_sidecar_label = TOOL_BROWSER_LABEL.to_string();

    let channel_tokens: ChannelTokenTable = tg.channel_tokens.clone();
    let shutdown = tg.shutdown.clone();

    let server = ChannelServer::bind(&tg.deps, port_file.clone(), channel_tokens.clone())
        .expect("bind ChannelServer");
    let port = server.port();

    let server_shutdown = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        let _ = server.run(server_shutdown).await;
    });

    // Mint the tool/browser-bound token. Held via `_handle` so it
    // stays in the table for the rest of the test.
    let _handle = channel_tokens.mint(ClientIdentity {
        pid: std::process::id(),
        label: TOOL_BROWSER_LABEL.into(),
        bound_channel_type: None,
    });
    let token = _handle.token().to_string();

    // Spawn a task that plays the sidecar: connect, Register,
    // RegisterAck, then loop replying to every RpcRequest.
    let sidecar_token = token.clone();
    let sidecar_task = tokio::spawn(async move { run_fake_sidecar(port, &sidecar_token).await });

    // Wait until the sidecar has registered with the gateway client
    // (the writer task inside the route handler calls
    // `client.attach(outbound)` immediately after Register success).
    wait_until(Duration::from_secs(2), || ws_client.is_attached())
        .await
        .expect("sidecar never attached to client");

    // Drive a real RPC through the public trait surface.
    let cancel = CancellationToken::new();
    let result = ws_client
        .call(
            "navigate",
            json!({ "url": "https://example.com/" }),
            "test-session",
            Duration::from_secs(2),
            cancel,
        )
        .await
        .expect("rpc call failed");

    // The fake sidecar echoes `params.url` back as `result.url` so we
    // can assert the round-trip carried both id correlation and the
    // params payload.
    assert_eq!(result["url"], "https://example.com/");
    assert_eq!(result["echoed_method"], "navigate");
    assert_eq!(result["echoed_context_id"], "test-session");

    // Cleanup. The fake sidecar's recv loop blocks forever waiting
    // on the next inbound frame, so abort it explicitly rather than
    // waiting for graceful shutdown to drain it.
    sidecar_task.abort();
    shutdown.trigger();
    let _ = server_handle.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_ws_returns_disconnected_when_no_sidecar_attached() {
    // No sidecar ever connects → call should fail fast.
    let client = WsBrowserSidecarClient::new();
    let cancel = CancellationToken::new();
    let err = client
        .call(
            "navigate",
            json!({ "url": "https://example.com/" }),
            "test-session",
            Duration::from_secs(1),
            cancel,
        )
        .await
        .expect_err("call must fail without an attached sidecar");
    let s = err.to_string();
    assert!(
        s.contains("not connected"),
        "expected disconnected, got: {s}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_ws_register_rejected_for_wrong_label() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let port_file =
        aura_workspace::WorkspacePaths::new(tempdir.path().to_path_buf()).channel_port();

    let mut tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let ws_client = WsBrowserSidecarClient::new();
    tg.deps.tool_ws_client = Some(ws_client.clone());
    tg.deps.tool_ws_sidecar_label = TOOL_BROWSER_LABEL.to_string();

    let channel_tokens = tg.channel_tokens.clone();
    let shutdown = tg.shutdown.clone();

    let server = ChannelServer::bind(&tg.deps, port_file.clone(), channel_tokens.clone())
        .expect("bind ChannelServer");
    let port = server.port();
    let sv_shutdown = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        let _ = server.run(sv_shutdown).await;
    });

    // Mint a token under a *different* label (a channel sidecar).
    // The route's `expected_label` check should reject the upgrade
    // even though the channel-auth middleware itself admits the token.
    let _handle = channel_tokens.mint(ClientIdentity {
        pid: std::process::id(),
        label: "telegram".into(),
        bound_channel_type: Some("telegram".into()),
    });
    let token = _handle.token().to_string();

    let outcome = connect_tool_ws(port, &token).await;
    // The handler should respond with a 403 and close the upgrade.
    assert!(outcome.is_err(), "expected upgrade failure, got Ok stream");

    shutdown.trigger();
    let _ = server_handle.await;
}

// ---------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------

type WsStream = tokio_tungstenite::WebSocketStream<TcpStream>;

async fn run_fake_sidecar(port: u16, token: &str) {
    let mut ws = match connect_tool_ws(port, token).await {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("fake sidecar connect failed: {e}");
            return;
        }
    };
    send_frame(
        &mut ws,
        &ToolFrame::Register {
            token: token.to_string(),
            sidecar_id: "browser".to_string(),
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .await;
    match recv_frame(&mut ws).await {
        Some(ToolFrame::RegisterAck { ok: true, .. }) => {}
        other => {
            eprintln!("fake sidecar got unexpected first frame: {other:?}");
            return;
        }
    }

    // Reply to every RpcRequest. Echoes the method + context_id and
    // forwards the original `params.url` back so the test can assert
    // round-trip integrity.
    while let Some(frame) = recv_frame(&mut ws).await {
        match frame {
            ToolFrame::RpcRequest { id, method, params } => {
                let result = json!({
                    "echoed_method": method,
                    "echoed_context_id": params.get("context_id"),
                    "url": params.get("url"),
                });
                send_frame(&mut ws, &ToolFrame::RpcResult { id, result }).await;
            }
            ToolFrame::Ping { ts } => {
                send_frame(&mut ws, &ToolFrame::Pong { ts }).await;
            }
            _ => {}
        }
    }
}

async fn connect_tool_ws(port: u16, token: &str) -> Result<WsStream, String> {
    let stream = TcpStream::connect(loopback(port))
        .await
        .map_err(|e| format!("dial: {e}"))?;
    let url = format!("ws://127.0.0.1:{port}/v1/tool-ws");
    let mut request = url
        .into_client_request()
        .map_err(|e| format!("request: {e}"))?;
    request.headers_mut().insert(
        CHANNEL_TOKEN_HEADER,
        token
            .parse()
            .map_err(|_| "invalid token header".to_string())?,
    );
    let (ws, _resp) = client_async(request, stream)
        .await
        .map_err(|e| format!("upgrade: {e}"))?;
    Ok(ws)
}

async fn send_frame(ws: &mut WsStream, frame: &ToolFrame) {
    let bytes = encode(frame).expect("encode");
    if let Err(e) = ws.send(WsMessage::Binary(bytes)).await {
        eprintln!("ws send failed: {e}");
    }
}

async fn recv_frame(ws: &mut WsStream) -> Option<ToolFrame> {
    loop {
        match ws.next().await? {
            Ok(WsMessage::Binary(bytes)) => return decode(&bytes).ok(),
            Ok(WsMessage::Close(_)) => return None,
            Ok(WsMessage::Ping(_)) | Ok(WsMessage::Pong(_)) | Ok(WsMessage::Frame(_)) => continue,
            Ok(WsMessage::Text(_)) => return None,
            Err(_) => return None,
        }
    }
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

async fn wait_until<F: Fn() -> bool>(max: Duration, cond: F) -> Result<(), ()> {
    let start = std::time::Instant::now();
    while start.elapsed() < max {
        if cond() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err(())
}

// `ws_client` exposes `attach`/`detach` but not a way to query
// attachment state from outside. The test crate gets an
// `is_attached_for_test` shim; inside the gateway crate this is a
// thin reflection on the inner state.
#[allow(dead_code)]
fn _force_arc_clone_check() {
    let _: Arc<dyn BrowserSidecarClient> = Arc::new(WsBrowserSidecarClient::new());
}
