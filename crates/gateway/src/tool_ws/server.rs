//! `GET /v1/tool-ws` route — accepts a tool sidecar connection,
//! validates the Register handshake, hooks the connection's outbound
//! queue into [`WsBrowserSidecarClient`], and runs the per-connection
//! read loop until close.
//!
//! Auth is delegated to the channel auth middleware
//! ([`crate::auth::channel`]). The route is mounted under the same
//! channel-auth wrapper as `/v1/channel-ws`, so the Register frame
//! itself only carries `sidecar_id` and `protocol_version` — no
//! redundant token round-trip. The `AuthedClient` extracted by the
//! middleware is required to be a `Subprocess` with the bound label
//! the gateway minted for this tool family.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message as AxumWsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, State};
use axum::response::IntoResponse;
use axum::routing::get;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;

use crate::auth::AuthedClient;

use super::client::WsBrowserSidecarClient;
use super::frame::{PROTOCOL_VERSION, ToolFrame, decode, encode};

const REGISTER_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on a single inbound WS frame (and message). 16 MiB is well
/// above any plausible RPC payload (the largest is `screenshot`
/// which is itself bounded to 16 MiB of decoded PNG → ~22 MiB of
/// b64 inside the frame, but screenshots flow gateway → sidecar
/// only as the *response*; on this inbound side the largest
/// realistic payload is a snapshot of a few hundred KB). A
/// hostile or buggy sidecar that sends a multi-GB frame would
/// otherwise be decoded into one Vec under axum's default
/// 64 MiB limit.
const MAX_WS_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct ToolWsState {
    /// Sidecar id the gateway expects in the Register frame. Today
    /// this is always `"browser"`; when a second tool sidecar lands
    /// we'll route by id and turn this into a registry.
    pub expected_sidecar_id: Arc<String>,
    /// Channel-auth label the connecting subprocess must present.
    /// Belt-and-braces with the WS-frame `sidecar_id` check — the
    /// label is what the token was bound to in `ChannelTokenTable`,
    /// so a sidecar minted under a different label can't reach this
    /// route even if it knows the token's hex string.
    pub expected_label: Arc<String>,
    /// The shared client; `attach`/`detach`/`dispatch_inbound` are
    /// called from within `run_connection`.
    pub client: WsBrowserSidecarClient,
}

pub fn routes() -> Router<ToolWsState> {
    Router::new().route("/tool-ws", get(ws_handler))
}

async fn ws_handler(
    State(state): State<ToolWsState>,
    Extension(authed): Extension<AuthedClient>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let label_ok = match &authed {
        AuthedClient::Subprocess { label, .. } => label.as_str() == state.expected_label.as_str(),
        AuthedClient::Tui => false,
    };
    if !label_ok {
        tracing::warn!(
            ?authed,
            expected = %state.expected_label,
            "tool-ws upgrade rejected: wrong client label",
        );
        return axum::http::StatusCode::FORBIDDEN.into_response();
    }
    ws.max_message_size(MAX_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_WS_MESSAGE_BYTES)
        .on_upgrade(move |socket| run_connection(socket, state))
        .into_response()
}

async fn run_connection(socket: WebSocket, state: ToolWsState) {
    let (mut sink, mut source) = socket.split();

    let register = match receive_register(&mut source).await {
        Ok(f) => f,
        Err(reason) => {
            send_ack_and_close(&mut sink, false, Some(reason.clone())).await;
            tracing::warn!(reason = %reason, "tool-ws handshake failed");
            return;
        }
    };

    let (sidecar_id, version) = match register {
        ToolFrame::Register {
            sidecar_id,
            protocol_version,
            ..
        } => (sidecar_id, protocol_version),
        other => {
            let reason = format!("expected Register, got {other:?}");
            send_ack_and_close(&mut sink, false, Some(reason.clone())).await;
            tracing::warn!(reason = %reason, "tool-ws first frame was not Register");
            return;
        }
    };

    if version != PROTOCOL_VERSION {
        let reason =
            format!("protocol version mismatch (gateway={PROTOCOL_VERSION}, sidecar={version})");
        send_ack_and_close(&mut sink, false, Some(reason.clone())).await;
        tracing::warn!(reason = %reason, "tool-ws protocol mismatch");
        return;
    }

    if sidecar_id.as_str() != state.expected_sidecar_id.as_str() {
        let reason = format!(
            "unknown sidecar_id `{sidecar_id}` (expected `{}`)",
            state.expected_sidecar_id
        );
        send_ack_and_close(&mut sink, false, Some(reason.clone())).await;
        tracing::warn!(reason = %reason, "tool-ws Register rejected");
        return;
    }

    let ack = ToolFrame::RegisterAck {
        ok: true,
        reason: None,
    };
    if let Err(e) = send_frame(&mut sink, &ack).await {
        tracing::warn!(error = %e, "tool-ws send RegisterAck failed");
        return;
    }

    tracing::info!(sidecar_id = %sidecar_id, "tool-ws sidecar registered");

    let (outbound_tx, mut outbound_rx) =
        mpsc::channel::<ToolFrame>(super::client::OUTBOUND_CHAN_CAPACITY);
    let attach_guard = state.client.attach(outbound_tx);

    let writer_task = tokio::spawn(async move {
        while let Some(frame) = outbound_rx.recv().await {
            if let Err(e) = send_frame(&mut sink, &frame).await {
                tracing::debug!(error = %e, "tool-ws writer: send failed; closing");
                break;
            }
        }
        let _ = sink.close().await;
    });

    run_inbound_loop(&mut source, &state.client).await;

    // Generation-tagged detach. Only clears the active outbound when
    // *this* connection's guard is still current; if a newer sidecar
    // has registered in the meantime, our close fires a no-op detach
    // and the new connection's queue stays live.
    state.client.detach(attach_guard);
    writer_task.abort();
    tracing::info!(sidecar_id = %sidecar_id, "tool-ws sidecar disconnected");
}

async fn run_inbound_loop(source: &mut SplitStream<WebSocket>, client: &WsBrowserSidecarClient) {
    while let Some(msg) = source.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(error = %e, "tool-ws read error");
                break;
            }
        };
        match msg {
            AxumWsMessage::Binary(bytes) => match decode(&bytes) {
                Ok(frame) => client.dispatch_inbound(frame),
                Err(e) => {
                    tracing::warn!(error = %e, "tool-ws decode failed");
                }
            },
            AxumWsMessage::Close(_) => break,
            AxumWsMessage::Ping(_) | AxumWsMessage::Pong(_) => continue,
            AxumWsMessage::Text(_) => {
                tracing::warn!("tool-ws unexpected text frame; closing");
                break;
            }
        }
    }
}

async fn receive_register(source: &mut SplitStream<WebSocket>) -> Result<ToolFrame, String> {
    let next = tokio::time::timeout(REGISTER_TIMEOUT, source.next())
        .await
        .map_err(|_| "timed out waiting for Register frame".to_string())?;
    let msg = next
        .ok_or_else(|| "peer closed before Register".to_string())?
        .map_err(|e| format!("ws error: {e}"))?;
    match msg {
        AxumWsMessage::Binary(bytes) => decode(&bytes),
        other => Err(format!("expected binary Register frame, got {other:?}")),
    }
}

async fn send_ack_and_close(
    sink: &mut SplitSink<WebSocket, AxumWsMessage>,
    ok: bool,
    reason: Option<String>,
) {
    let frame = ToolFrame::RegisterAck { ok, reason };
    if let Err(e) = send_frame(sink, &frame).await {
        tracing::debug!(error = %e, "tool-ws send RegisterAck failed");
    }
    let _ = sink.close().await;
}

async fn send_frame(
    sink: &mut SplitSink<WebSocket, AxumWsMessage>,
    frame: &ToolFrame,
) -> Result<(), String> {
    let bytes = encode(frame).map_err(|e| format!("encode: {e}"))?;
    sink.send(AxumWsMessage::Binary(bytes.into()))
        .await
        .map_err(|e| format!("send: {e}"))
}
