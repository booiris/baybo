//! Channel UDS listener end-to-end.
//!
//! Binds a real `ChannelServer` against a temp-dir socket, then drives
//! the session/message/stream flow from a hyper UDS client that
//! authenticates with the TUI PSK. Exercises the full stack: peer-cred
//! extraction, `auth_channel` middleware, the channel router, and the
//! SSE fan-out `HttpAdapter::send` publishes through.

use std::sync::Arc;
use std::time::Duration;

use aura_agent::service::ShutdownSignal;
use aura_channels::{AgentOutput, ChannelAdapter, NoticeLevel};
use aura_gateway::test_support::build_test_deps;
use aura_gateway::uds::ChannelServer;
use aura_gateway_auth::TUI_PSK_HEADER;
use aura_model::ChannelType;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::UnixStream;

const TEST_PSK: [u8; 32] = [0x42; 32];

async fn spawn_channel_server() -> ChannelServerFixture {
    let mut tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    // The channel server binds the UDS immediately; bookkeeping for
    // the tempdir lives on the fixture so the socket file outlives the
    // server only long enough for cleanup.
    let tempdir = tempfile::tempdir().expect("tempdir");
    let socket_path = tempdir.path().join("channel.sock");
    let tokens = tg.channel_tokens.clone();
    let shutdown = tg.shutdown.clone();
    let server = ChannelServer::bind(&tg.deps, socket_path.clone(), TEST_PSK, tokens)
        .expect("bind ChannelServer");
    let adapter = Arc::clone(&tg.adapter);
    let run_shutdown = shutdown.clone();
    let handle = tokio::spawn(async move {
        let _ = server.run(run_shutdown).await;
    });
    // Keep the TestGateway's tempdir alive by moving it into the fixture.
    tg.shutdown = shutdown.clone();
    ChannelServerFixture {
        socket_path,
        shutdown,
        adapter,
        _tempdir: tempdir,
        _gateway: tg,
        handle,
    }
}

struct ChannelServerFixture {
    socket_path: std::path::PathBuf,
    shutdown: ShutdownSignal,
    adapter: Arc<aura_gateway::HttpAdapter>,
    _tempdir: tempfile::TempDir,
    _gateway: aura_gateway::test_support::TestGateway,
    handle: tokio::task::JoinHandle<()>,
}

impl ChannelServerFixture {
    async fn stop(self) {
        self.shutdown.trigger();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.handle).await;
    }
}

/// Send one HTTP/1.1 request over a fresh UnixStream with the TUI PSK
/// header set. Returns the response parts + body bytes.
async fn send_uds_request(
    socket_path: &std::path::Path,
    method: Method,
    path: &str,
    body: Bytes,
) -> (StatusCode, Vec<u8>) {
    let resp = open_and_send(socket_path, method, path, body, None).await;
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await.expect("read body").to_bytes().to_vec();
    (parts.status, bytes)
}

async fn open_and_send(
    socket_path: &std::path::Path,
    method: Method,
    path: &str,
    body: Bytes,
    accept: Option<&'static str>,
) -> Response<Incoming> {
    let stream = UnixStream::connect(socket_path).await.expect("connect uds");
    let io = TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .expect("handshake");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let mut builder = Request::builder()
        .method(&method)
        .uri(path)
        .header("host", "aura.local")
        .header(TUI_PSK_HEADER, hex::encode(TEST_PSK));
    if let Some(accept) = accept {
        builder = builder.header("accept", accept);
    }
    if !body.is_empty() {
        builder = builder.header("content-type", "application/json");
    }
    let req = builder.body(Full::new(body)).expect("build req");
    sender.send_request(req).await.expect("send_request")
}

#[tokio::test]
async fn healthz_is_open_on_channel_socket() {
    let fx = spawn_channel_server().await;
    let (status, _body) =
        send_uds_request(&fx.socket_path, Method::GET, "/healthz", Bytes::new()).await;
    assert_eq!(status, StatusCode::OK);
    fx.stop().await;
}

#[tokio::test]
async fn session_create_message_and_stream_round_trip() {
    let fx = spawn_channel_server().await;

    // Wire the adapter to accept submits: the router intake channel
    // would normally be driven by the Router, but the adapter's
    // `submit` requires `start` to have been called. Attach a dummy
    // receiver so the submit path does not fail.
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    fx.adapter.start(tx).await.expect("start adapter");

    // 1. POST /v1/sessions → create a session.
    let (status, body) = send_uds_request(
        &fx.socket_path,
        Method::POST,
        "/v1/sessions",
        Bytes::from_static(b"{}"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create session returned {status}: {}",
        String::from_utf8_lossy(&body)
    );
    let session: Value = serde_json::from_slice(&body).expect("session json");
    let session_id = session["id"].as_str().expect("id").to_string();
    assert!(!session_id.is_empty());

    // 2. GET /v1/sessions → the fresh session is listed.
    let (status, body) =
        send_uds_request(&fx.socket_path, Method::GET, "/v1/sessions", Bytes::new()).await;
    assert_eq!(status, StatusCode::OK);
    let list: Value = serde_json::from_slice(&body).expect("list json");
    let items = list["items"].as_array().expect("items");
    assert!(
        items
            .iter()
            .any(|item| item["id"].as_str() == Some(&session_id))
    );

    // 3. Open SSE subscription BEFORE publishing deltas so the
    //    broadcast sender has an active subscriber.
    let sse_resp = open_and_send(
        &fx.socket_path,
        Method::GET,
        &format!("/v1/sessions/{session_id}/stream"),
        Bytes::new(),
        Some("text/event-stream"),
    )
    .await;
    assert_eq!(sse_resp.status(), StatusCode::OK);
    let (_parts, mut sse_body) = sse_resp.into_parts();

    // Give the subscribe handler a tick to actually register with the
    // adapter's broadcast before we publish — otherwise the delta fires
    // before the subscriber and the broadcast drops it.
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;

    // 4. Publish a delta through the adapter so the SSE body has
    //    something to emit. We don't drive the full agent loop here —
    //    the interesting contract is that `send_stream_delta` reaches
    //    the UDS subscriber.
    fx.adapter
        .send(AgentOutput::Delta {
            session_id: session_id.clone(),
            channel: ChannelType::http(),
            text: "hello from test".into(),
        })
        .await
        .expect("delta");
    fx.adapter
        .send(AgentOutput::Notice {
            session_id: session_id.clone(),
            channel: ChannelType::http(),
            level: NoticeLevel::Warn,
            text: "world".into(),
        })
        .await
        .expect("notice");

    // 5. Drain enough of the SSE body to see both events.
    let text = read_sse_chunks_until(&mut sse_body, "world", Duration::from_secs(3)).await;
    assert!(text.contains("hello from test"), "missing delta: {text}");
    assert!(text.contains("event: delta"), "missing delta frame: {text}");
    assert!(
        text.contains("event: notice"),
        "missing notice frame: {text}"
    );

    // 6. POST /v1/sessions/{id}/messages — the router would normally
    //    dispatch this, but we're only verifying the channel listener
    //    accepts the POST and hands it to the adapter (router drain
    //    asserted below via `rx.recv`).
    let (status, _body) = send_uds_request(
        &fx.socket_path,
        Method::POST,
        &format!("/v1/sessions/{session_id}/messages"),
        Bytes::from(serde_json::to_vec(&json!({"text": "ping"})).unwrap()),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let router_msg = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("router intake timeout")
        .expect("router intake closed");
    assert_eq!(router_msg.message.session_id, session_id);

    // 7. DELETE the session.
    let (status, _body) = send_uds_request(
        &fx.socket_path,
        Method::DELETE,
        &format!("/v1/sessions/{session_id}"),
        Bytes::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    fx.stop().await;
}

#[tokio::test]
async fn missing_psk_header_is_unauthorized() {
    let fx = spawn_channel_server().await;
    let stream = UnixStream::connect(&fx.socket_path).await.expect("connect");
    let io = TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .expect("handshake");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    // No PSK header — middleware must reject.
    let req = Request::builder()
        .method(Method::GET)
        .uri("/v1/sessions")
        .header("host", "aura.local")
        .body(Full::new(Bytes::new()))
        .expect("build");
    let resp = sender.send_request(req).await.expect("send");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    fx.stop().await;
}

#[tokio::test]
async fn wrong_psk_is_unauthorized() {
    let fx = spawn_channel_server().await;
    let stream = UnixStream::connect(&fx.socket_path).await.expect("connect");
    let io = TokioIo::new(stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .expect("handshake");
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let req = Request::builder()
        .method(Method::GET)
        .uri("/v1/sessions")
        .header("host", "aura.local")
        .header(TUI_PSK_HEADER, hex::encode([0u8; 32]))
        .body(Full::new(Bytes::new()))
        .expect("build");
    let resp = sender.send_request(req).await.expect("send");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    fx.stop().await;
}

/// Read SSE body chunks until we've seen `sentinel` in the accumulated
/// text or `deadline` elapses. Keeps polling frame-by-frame so a fast
/// flush doesn't require us to block on `Incoming::collect` (which
/// would only return when the body ends).
async fn read_sse_chunks_until(body: &mut Incoming, sentinel: &str, deadline: Duration) -> String {
    use http_body_util::BodyExt;
    let start = tokio::time::Instant::now();
    let mut text = String::new();
    while tokio::time::Instant::now().duration_since(start) < deadline {
        let remaining = deadline.saturating_sub(tokio::time::Instant::now().duration_since(start));
        match tokio::time::timeout(remaining, body.frame()).await {
            Ok(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    text.push_str(&String::from_utf8_lossy(data));
                    if text.contains(sentinel) {
                        return text;
                    }
                }
            }
            Ok(Some(Err(e))) => panic!("frame error: {e}"),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    text
}
