//! `GET /v1/browser/view/ws` — the web dashboard's read-only live view of
//! the agent's browser.
//!
//! Mounted on the admin v1 router, so it sits behind `require_admin_token`
//! (Bearer or `?token=`, stripped before the `TraceLayer` logs the URI).
//! Text messages are JSON `ViewerDown` / `ViewerUp`; binary messages are
//! screencast frames, `[u32 BE hdr_len][FrameHeader JSON][JPEG]`, forwarded
//! from the sidecar link byte for byte. The viewer must `Ping` at the
//! interval its first `Link` message announces or the socket is closed.
//! No OpenAPI entry: a WS upgrade has no useful schema (same as
//! `/v1/channel-ws`).

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use baybo_browser_view::error::ViewError;
use baybo_browser_view::hub::{BrowserViewer, ViewEvent};
use baybo_browser_view::limits::{
    MAX_BROWSER_VIEW_CLIENT_MSG_BYTES, VIEWER_PING_TIMEOUT, WS_SEND_TIMEOUT,
};
use baybo_browser_view::wire::{ViewerDown, ViewerErrorCode, ViewerUp};
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use tokio::time::Instant;
use utoipa_axum::router::OpenApiRouter;

use crate::auth::AuthedClient;
use crate::server::AdminState;

/// Route path under `/v1`.
pub(crate) const BROWSER_VIEW_WS_PATH: &str = "/browser/view/ws";

const CLOSE_REASON_SHUTDOWN: &str = "gateway shutting down";
const CLOSE_REASON_PING_TIMEOUT: &str = "no ping";
const CLOSE_REASON_TOO_MANY_VIEWERS: &str = "too many viewers";

type ViewerSink = SplitSink<WebSocket, Message>;

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new().route(BROWSER_VIEW_WS_PATH, get(ws_handler))
}

async fn ws_handler(
    State(state): State<AdminState>,
    Extension(authed): Extension<AuthedClient>,
    ws: WebSocketUpgrade,
) -> Response {
    if !may_view(&authed) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let viewer = state.browser_viewer.clone();
    ws.max_message_size(MAX_BROWSER_VIEW_CLIENT_MSG_BYTES)
        .max_frame_size(MAX_BROWSER_VIEW_CLIENT_MSG_BYTES)
        .on_upgrade(move |socket| run_viewer(socket, viewer))
        .into_response()
}

/// Only the dashboard (admin bearer) and paired devices may watch. Admin auth
/// yields nothing else today; the explicit match keeps a future identity
/// that reaches this route from being let in by default.
fn may_view(authed: &AuthedClient) -> bool {
    match authed {
        AuthedClient::Web | AuthedClient::Device { .. } => true,
        AuthedClient::Tui | AuthedClient::Tool { .. } | AuthedClient::Subprocess { .. } => false,
    }
}

async fn run_viewer(socket: WebSocket, viewer: BrowserViewer) {
    let (mut sink, mut source) = socket.split();
    let mut subscription = match viewer.subscribe() {
        Ok(subscription) => subscription,
        Err(ViewError::TooManyViewers { max }) => {
            tracing::warn!(max, "browser view: refusing a viewer over the limit");
            let error = ViewerDown::Error {
                code: ViewerErrorCode::TooManyViewers,
            };
            if send_json(&mut sink, &error).await {
                close(&mut sink, close_code::POLICY, CLOSE_REASON_TOO_MANY_VIEWERS).await;
            }
            return;
        }
    };
    tracing::debug!("browser view: viewer connected");
    let mut ping_deadline = Instant::now() + VIEWER_PING_TIMEOUT;
    loop {
        let delivered = tokio::select! {
            event = subscription.recv() => match event {
                ViewEvent::State(msg) => send_json(&mut sink, &msg).await,
                ViewEvent::Frame(frame) => send(&mut sink, Message::Binary(frame)).await,
                ViewEvent::Shutdown => {
                    close(&mut sink, close_code::AWAY, CLOSE_REASON_SHUTDOWN).await;
                    break;
                }
            },
            inbound = source.next() => match inbound {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<ViewerUp>(&text) {
                    Ok(ViewerUp::Ping) => {
                        ping_deadline = Instant::now() + VIEWER_PING_TIMEOUT;
                        send_json(&mut sink, &ViewerDown::Pong).await
                    }
                    Ok(ViewerUp::RequestControl) => {
                        let error = ViewerDown::Error { code: ViewerErrorCode::TakeoverDisabled };
                        send_json(&mut sink, &error).await
                    }
                    Err(_) => {
                        let error = ViewerDown::Error { code: ViewerErrorCode::BadMessage };
                        send_json(&mut sink, &error).await
                    }
                },
                Some(Ok(Message::Binary(_))) => {
                    let error = ViewerDown::Error { code: ViewerErrorCode::BadMessage };
                    send_json(&mut sink, &error).await
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => true,
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(e)) => {
                    tracing::debug!(error = %e, "browser view: viewer socket error");
                    break;
                }
            },
            _ = tokio::time::sleep_until(ping_deadline) => {
                tracing::debug!("browser view: closing a viewer that stopped pinging");
                close(&mut sink, close_code::POLICY, CLOSE_REASON_PING_TIMEOUT).await;
                break;
            }
        };
        if !delivered {
            break;
        }
    }
    // Flushes the close reply tungstenite queued for a viewer-initiated
    // close; without it the viewer sees 1006 instead of its own code.
    let _ = tokio::time::timeout(WS_SEND_TIMEOUT, sink.close()).await;
    tracing::debug!("browser view: viewer disconnected");
}

async fn send_json(sink: &mut ViewerSink, msg: &ViewerDown) -> bool {
    match serde_json::to_string(msg) {
        Ok(text) => send(sink, Message::Text(text.into())).await,
        Err(e) => {
            tracing::warn!(error = %e, "browser view: could not encode a viewer message");
            false
        }
    }
}

/// `false` when the viewer is gone or too slow; the caller then drops it.
async fn send(sink: &mut ViewerSink, msg: Message) -> bool {
    match tokio::time::timeout(WS_SEND_TIMEOUT, sink.send(msg)).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "browser view: send to viewer failed");
            false
        }
        Err(_) => {
            tracing::debug!("browser view: send to viewer timed out");
            false
        }
    }
}

async fn close(sink: &mut ViewerSink, code: u16, reason: &'static str) {
    send(
        sink,
        Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_dashboard_and_devices_may_view() {
        assert!(may_view(&AuthedClient::Web));
        assert!(may_view(&AuthedClient::Device {
            device_id: "dev".into()
        }));
        assert!(!may_view(&AuthedClient::Tui));
        assert!(!may_view(&AuthedClient::Tool {
            label: "tool/browser".into()
        }));
        assert!(!may_view(&AuthedClient::Subprocess {
            pid: 1,
            label: "telegram".into(),
            channel_type: None,
        }));
    }
}
