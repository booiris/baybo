//! `GET /v1/browser/view/ws` on the real admin router, fed by a fake
//! browser sidecar speaking the link protocol over the hub's unix socket.

use std::time::Duration;

use baybo_browser_view::codec::{SidecarLinkCodec, SidecarMessage};
use baybo_browser_view::limits::{
    BROWSER_LINK_PROTOCOL_VERSION, MAX_BROWSER_VIEW_CLIENT_MSG_BYTES, MAX_VIEWERS,
    VIEWER_PING_TIMEOUT,
};
use baybo_browser_view::params::BrowserLinkParams;
use baybo_browser_view::wire::{
    BootId, BrowserMode, BrowserPhase, BrowserStatus, Capability, FrameHeader, LinkDown, LinkUp,
    TargetId, UnavailableReason, ViewerDown, ViewerErrorCode, ViewerUp,
};
use baybo_gateway::server::build_admin_router_for_tests;
use baybo_gateway::test_support::{
    TEST_ADMIN_TOKEN, TestGateway, build_test_deps, build_test_deps_with_browser_view,
    build_test_deps_with_unlinked_browser_view,
};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream, UnixStream};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{WebSocketStream, client_async};
use tokio_util::codec::Framed;

const WAIT: Duration = Duration::from_secs(10);
const PATH: &str = "/v1/browser/view/ws";

type Viewer = WebSocketStream<TcpStream>;
type Sidecar = Framed<UnixStream, SidecarLinkCodec>;

async fn serve(tg: &TestGateway) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let router = build_admin_router_for_tests(&tg.deps);
    tokio::spawn(async move {
        let _ = axum::serve(listener, router.into_make_service()).await;
    });
    port
}

async fn try_connect(
    port: u16,
    query: &str,
) -> Result<Viewer, tokio_tungstenite::tungstenite::Error> {
    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let req = format!("ws://127.0.0.1:{port}{PATH}{query}")
        .into_client_request()
        .unwrap();
    client_async(req, stream).await.map(|(ws, _)| ws)
}

async fn connect(port: u16) -> Viewer {
    try_connect(port, &format!("?token={TEST_ADMIN_TOKEN}"))
        .await
        .expect("viewer upgrade")
}

async fn next_ws(viewer: &mut Viewer) -> Option<WsMessage> {
    tokio::time::timeout(WAIT, viewer.next())
        .await
        .expect("viewer message in time")
        .and_then(Result::ok)
}

async fn next_text(viewer: &mut Viewer) -> ViewerDown {
    loop {
        match next_ws(viewer).await {
            Some(WsMessage::Text(text)) => return serde_json::from_str(&text).unwrap(),
            Some(WsMessage::Ping(_) | WsMessage::Pong(_)) => {}
            other => panic!("expected a text message, got {other:?}"),
        }
    }
}

async fn send_up(viewer: &mut Viewer, up: &ViewerUp) {
    viewer
        .send(WsMessage::Text(serde_json::to_string(up).unwrap()))
        .await
        .unwrap();
}

/// Reads until the server closes; returns the close code it sent, if any.
async fn close_code(viewer: &mut Viewer) -> Option<CloseCode> {
    loop {
        match tokio::time::timeout(WAIT, viewer.next())
            .await
            .expect("close in time")
        {
            Some(Ok(WsMessage::Close(frame))) => return frame.map(|f| f.code),
            Some(Ok(_)) => {}
            Some(Err(_)) | None => return None,
        }
    }
}

async fn sidecar(link: &BrowserLinkParams) -> Sidecar {
    let stream = UnixStream::connect(link.socket()).await.unwrap();
    let mut sidecar = Framed::new(stream, SidecarLinkCodec);
    sidecar
        .send(SidecarMessage::Json(LinkUp::Hello {
            protocol: BROWSER_LINK_PROTOCOL_VERSION,
            secret: link.secret().clone(),
            boot_id: BootId::new("boot-1"),
            pid: 1,
            capabilities: vec![Capability::Screencast],
        }))
        .await
        .unwrap();
    match next_link(&mut sidecar).await {
        LinkDown::HelloAck { protocol, .. } => assert_eq!(protocol, BROWSER_LINK_PROTOCOL_VERSION),
        other => panic!("expected a HelloAck, got {other:?}"),
    }
    sidecar
}

async fn next_link(sidecar: &mut Sidecar) -> LinkDown {
    tokio::time::timeout(WAIT, sidecar.next())
        .await
        .expect("link message in time")
        .expect("link open")
        .unwrap()
}

fn status() -> BrowserStatus {
    BrowserStatus {
        mode: BrowserMode::Host,
        phase: BrowserPhase::Ready,
        browser_gen: 1,
    }
}

fn header() -> FrameHeader {
    FrameHeader {
        target_id: TargetId::new("T1"),
        browser_gen: 1,
        seq: 3,
        captured_at_ms: 1.5,
        device_width: 1280.0,
        device_height: 800.0,
        offset_top: 0.0,
        page_scale_factor: 1.0,
        scroll_offset_x: 0.0,
        scroll_offset_y: 10.0,
    }
}

#[tokio::test]
async fn upgrade_without_token_is_401() {
    let tg = build_test_deps_with_browser_view("127.0.0.1:0".parse().unwrap()).await;
    let port = serve(&tg).await;
    match try_connect(port, "").await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status().as_u16(), 401);
        }
        other => panic!("expected 401, got {other:?}"),
    }
    match try_connect(port, "?token=wrong").await {
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            assert_eq!(resp.status().as_u16(), 401);
        }
        other => panic!("expected 401, got {other:?}"),
    }
}

#[tokio::test]
async fn disabled_view_reports_browser_disabled() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    assert!(tg.browser_link.is_none());
    let port = serve(&tg).await;
    let mut viewer = connect(port).await;
    assert_eq!(
        next_text(&mut viewer).await,
        ViewerDown::link(false, 0, Some(UnavailableReason::BrowserDisabled))
    );
}

#[tokio::test]
async fn unlinked_view_reports_down_without_a_reason_and_gives_no_link() {
    let tg = build_test_deps_with_unlinked_browser_view("127.0.0.1:0".parse().unwrap()).await;
    assert!(tg.browser_link.is_none(), "no link env for the sidecar");
    let port = serve(&tg).await;
    let mut viewer = connect(port).await;
    assert_eq!(
        next_text(&mut viewer).await,
        ViewerDown::link(false, 0, None)
    );
}

#[tokio::test]
async fn snapshot_on_connect_and_frames_arrive_verbatim() {
    let tg = build_test_deps_with_browser_view("127.0.0.1:0".parse().unwrap()).await;
    let link = tg.browser_link.clone().expect("link bound");
    let port = serve(&tg).await;

    let mut first = connect(port).await;
    assert_eq!(
        next_text(&mut first).await,
        ViewerDown::link(false, 0, None)
    );

    let mut fake = sidecar(&link).await;
    assert_eq!(next_link(&mut fake).await, LinkDown::StartScreencast);
    assert_eq!(next_text(&mut first).await, ViewerDown::link(true, 1, None));
    fake.send(SidecarMessage::Json(LinkUp::Status(status())))
        .await
        .unwrap();
    assert_eq!(next_text(&mut first).await, ViewerDown::Status(status()));

    let mut second = connect(port).await;
    assert_eq!(
        next_text(&mut second).await,
        ViewerDown::link(true, 1, None)
    );
    assert_eq!(next_text(&mut second).await, ViewerDown::Status(status()));

    let jpeg = Bytes::from_static(b"\xFF\xD8not-really-a-jpeg\xFF\xD9");
    fake.send(SidecarMessage::Frame {
        header: header(),
        jpeg: jpeg.clone(),
    })
    .await
    .unwrap();
    let header_json = serde_json::to_vec(&header()).unwrap();
    let mut expected = (header_json.len() as u32).to_be_bytes().to_vec();
    expected.extend_from_slice(&header_json);
    expected.extend_from_slice(&jpeg);
    for viewer in [&mut first, &mut second] {
        match next_ws(viewer).await {
            Some(WsMessage::Binary(bytes)) => assert_eq!(bytes, expected),
            other => panic!("expected a binary frame, got {other:?}"),
        }
    }

    drop(first);
    drop(second);
    assert_eq!(next_link(&mut fake).await, LinkDown::StopScreencast);
}

#[tokio::test]
async fn viewer_close_gets_its_code_echoed() {
    let tg = build_test_deps_with_browser_view("127.0.0.1:0".parse().unwrap()).await;
    let port = serve(&tg).await;
    let mut viewer = connect(port).await;
    assert!(matches!(
        next_text(&mut viewer).await,
        ViewerDown::Link { .. }
    ));
    viewer
        .close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
            code: CloseCode::Normal,
            reason: "bye".into(),
        }))
        .await
        .unwrap();
    assert_eq!(close_code(&mut viewer).await, Some(CloseCode::Normal));
}

#[tokio::test]
async fn ping_pong_and_takeover_disabled() {
    let tg = build_test_deps_with_browser_view("127.0.0.1:0".parse().unwrap()).await;
    let port = serve(&tg).await;
    let mut viewer = connect(port).await;
    assert!(matches!(
        next_text(&mut viewer).await,
        ViewerDown::Link { .. }
    ));
    send_up(&mut viewer, &ViewerUp::Ping).await;
    assert_eq!(next_text(&mut viewer).await, ViewerDown::Pong);
    send_up(&mut viewer, &ViewerUp::RequestControl).await;
    assert_eq!(
        next_text(&mut viewer).await,
        ViewerDown::Error {
            code: ViewerErrorCode::TakeoverDisabled
        }
    );
    viewer
        .send(WsMessage::Text(r#"{"type":"nope"}"#.into()))
        .await
        .unwrap();
    assert_eq!(
        next_text(&mut viewer).await,
        ViewerDown::Error {
            code: ViewerErrorCode::BadMessage
        }
    );
}

#[tokio::test]
async fn oversized_client_message_disconnects() {
    let tg = build_test_deps_with_browser_view("127.0.0.1:0".parse().unwrap()).await;
    let port = serve(&tg).await;
    let mut viewer = connect(port).await;
    assert!(matches!(
        next_text(&mut viewer).await,
        ViewerDown::Link { .. }
    ));
    let fits = format!(
        r#"{{"type":"ping","pad":"{}"}}"#,
        "x".repeat(MAX_BROWSER_VIEW_CLIENT_MSG_BYTES / 2)
    );
    viewer.send(WsMessage::Text(fits)).await.unwrap();
    assert_eq!(
        next_text(&mut viewer).await,
        ViewerDown::Pong,
        "a message under the limit is read and answered"
    );
    let big = "x".repeat(MAX_BROWSER_VIEW_CLIENT_MSG_BYTES + 1024);
    let _ = viewer.send(WsMessage::Text(big)).await;
    // A capacity error leaves the server's socket unusable, so it usually
    // drops without a close frame. The answered half-limit message above is
    // what pins the size as the reason.
    let code = close_code(&mut viewer).await;
    assert!(
        matches!(
            code,
            None | Some(CloseCode::Size) | Some(CloseCode::Protocol)
        ),
        "{code:?}"
    );
}

#[tokio::test]
async fn viewer_without_ping_is_closed() {
    let tg = build_test_deps_with_browser_view("127.0.0.1:0".parse().unwrap()).await;
    let port = serve(&tg).await;
    let mut viewer = connect(port).await;
    assert!(matches!(
        next_text(&mut viewer).await,
        ViewerDown::Link { .. }
    ));
    // Paused only now: the boot path above has real-time timers of its own.
    // Every timer left is the server's ping deadline or ours, set longer.
    tokio::time::pause();
    let closed = tokio::time::timeout(2 * VIEWER_PING_TIMEOUT, async {
        loop {
            match viewer.next().await {
                Some(Ok(WsMessage::Close(frame))) => return frame.map(|f| f.code),
                Some(Ok(_)) => {}
                Some(Err(_)) | None => return None,
            }
        }
    })
    .await
    .expect("closed before our own deadline");
    assert_eq!(closed, Some(CloseCode::Policy));
}

#[tokio::test]
async fn too_many_viewers_get_an_error_and_close() {
    let tg = build_test_deps_with_browser_view("127.0.0.1:0".parse().unwrap()).await;
    let port = serve(&tg).await;
    let mut viewers = Vec::new();
    for _ in 0..MAX_VIEWERS {
        let mut viewer = connect(port).await;
        assert!(matches!(
            next_text(&mut viewer).await,
            ViewerDown::Link { .. }
        ));
        viewers.push(viewer);
    }
    let mut extra = connect(port).await;
    assert_eq!(
        next_text(&mut extra).await,
        ViewerDown::Error {
            code: ViewerErrorCode::TooManyViewers
        }
    );
    assert_eq!(close_code(&mut extra).await, Some(CloseCode::Policy));
}

#[tokio::test]
async fn shutdown_closes_viewers_with_1001() {
    let tg = build_test_deps_with_browser_view("127.0.0.1:0".parse().unwrap()).await;
    let port = serve(&tg).await;
    let mut viewer = connect(port).await;
    assert!(matches!(
        next_text(&mut viewer).await,
        ViewerDown::Link { .. }
    ));
    tg.shutdown.trigger();
    assert_eq!(close_code(&mut viewer).await, Some(CloseCode::Away));
}
