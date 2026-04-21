//! `GET /v1/channel-ws` route — upgrades a channel-auth'd request into
//! a WebSocket, runs the Register handshake, registers a
//! [`super::adapter::Sidecar`]'s [`aura_channels::Channel`] with the
//! workspace [`ChannelRegistry`], and drives the per-connection
//! inbound loop until either side closes.

use std::time::Duration;

use aura_channels::wire::{self, Frame};
use aura_channels::{ChannelError, IncomingMessage, Message as AgentMessage};
use aura_model::{ChannelType, ContentBlock, MessageMetadata, User};
use axum::Router;
use axum::extract::ws::{Message as AxumWsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, State};
use axum::response::IntoResponse;
use axum::routing::get;
use chrono::Utc;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};

use super::adapter::Sidecar;
use super::handshake::validate_register;
use super::state::WsChannelState;
use crate::auth_channel::AuthedClient;

/// Maximum time to wait for the client's `Register` frame after the WS
/// upgrade completes. Keeps idle connections that never speak from
/// pinning a registry slot.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(5);

pub fn routes() -> Router<WsChannelState> {
    Router::new().route("/channel-ws", get(ws_handler))
}

async fn ws_handler(
    State(state): State<WsChannelState>,
    Extension(authed): Extension<AuthedClient>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| run_connection(socket, state, authed))
        .into_response()
}

async fn run_connection(socket: WebSocket, state: WsChannelState, authed: AuthedClient) {
    let (mut sink, mut source) = socket.split();

    let channel_type = match receive_register(&mut source).await {
        Ok(frame) => match validate_register(frame, &authed, &state.tokens) {
            Ok(ct) => ct,
            Err(reason) => {
                send_ack_and_close(&mut sink, false, Some(reason.clone())).await;
                tracing::warn!(reason = %reason, "channel-ws register rejected");
                return;
            }
        },
        Err(reason) => {
            send_ack_and_close(&mut sink, false, Some(reason.clone())).await;
            tracing::warn!(reason = %reason, "channel-ws handshake failed");
            return;
        }
    };

    let sidecar = Sidecar::build(channel_type.clone(), sink);

    if let Err(err) = state
        .registry
        .register(std::sync::Arc::clone(&sidecar.channel))
    {
        let reason = match &err {
            ChannelError::DuplicateChannel(ct) => format!("channel '{ct}' already registered"),
            other => format!("registration failed: {other}"),
        };
        if let Err(e) = sidecar
            .send_frame(Frame::RegisterAck {
                ok: false,
                reason: Some(reason.clone()),
            })
            .await
        {
            tracing::debug!(error = %e, "failed to send duplicate-register ack");
        }
        tracing::warn!(reason = %reason, "channel-ws register rejected");
        let _ = sidecar.into_pump().await;
        return;
    }

    tracing::info!(channel_type = %channel_type, "channel-ws sidecar registered");

    if let Err(e) = sidecar
        .send_frame(Frame::RegisterAck {
            ok: true,
            reason: None,
        })
        .await
    {
        tracing::warn!(error = %e, "failed to send RegisterAck");
        let _ = state.registry.unregister(channel_type);
        let _ = sidecar.into_pump().await;
        return;
    }

    run_inbound_loop(source, &state, &channel_type, &sidecar).await;

    if let Err(e) = state.registry.unregister(channel_type.clone()) {
        tracing::debug!(error = %e, %channel_type, "unregister after ws drop");
    }
    let _ = sidecar.into_pump().await;
    tracing::info!(%channel_type, "channel-ws sidecar disconnected");
}

async fn receive_register(
    source: &mut SplitStream<WebSocket>,
) -> std::result::Result<Frame, String> {
    let next = tokio::time::timeout(REGISTER_TIMEOUT, source.next())
        .await
        .map_err(|_| "timed out waiting for Register frame".to_string())?;
    let msg = next
        .ok_or_else(|| "peer closed before Register".to_string())?
        .map_err(|e| format!("ws error: {e}"))?;
    match msg {
        AxumWsMessage::Binary(bytes) => {
            wire::decode(&bytes).map_err(|e| format!("decode frame: {e}"))
        }
        other => Err(format!("expected binary frame, got {other:?}")),
    }
}

async fn send_ack_and_close(
    sink: &mut SplitSink<WebSocket, AxumWsMessage>,
    ok: bool,
    reason: Option<String>,
) {
    let frame = Frame::RegisterAck { ok, reason };
    match wire::encode(&frame) {
        Ok(bytes) => {
            if let Err(e) = sink.send(AxumWsMessage::Binary(bytes.into())).await {
                tracing::debug!(error = %e, "failed to send RegisterAck");
            }
        }
        Err(e) => tracing::error!(error = %e, "encode RegisterAck"),
    }
    let _ = sink.close().await;
}

async fn run_inbound_loop(
    mut source: SplitStream<WebSocket>,
    state: &WsChannelState,
    channel_type: &ChannelType,
    sidecar: &Sidecar,
) {
    while let Some(msg) = source.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(error = %e, "ws read error; tearing down");
                break;
            }
        };
        match msg {
            AxumWsMessage::Binary(bytes) => {
                let frame = match wire::decode(&bytes) {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!(error = %e, "decode frame failed");
                        continue;
                    }
                };
                match frame {
                    Frame::Message(wire_msg) => {
                        let sender = User {
                            id: wire_msg.user_id.clone(),
                            name: None,
                            channel: channel_type.clone(),
                        };
                        let incoming = IncomingMessage {
                            message: AgentMessage {
                                id: uuid::Uuid::new_v4().to_string(),
                                session_id: wire_msg.session_id,
                                channel: channel_type.clone(),
                                sender,
                                content: vec![ContentBlock::Text(wire_msg.content)],
                                timestamp: Utc::now(),
                                reply_to: None,
                                metadata: MessageMetadata::default(),
                            },
                        };
                        if let Err(e) = state.incoming_tx.send(incoming).await {
                            tracing::error!(error = %e, "router intake closed; tearing down");
                            break;
                        }
                    }
                    Frame::ResolveApproval { call_id, decision } => {
                        let resolved = sidecar.resolve_approval(&call_id, decision).await;
                        if !resolved {
                            tracing::debug!(
                                call_id = %call_id,
                                "ResolveApproval for unknown call_id; ignored"
                            );
                        }
                    }
                    other => {
                        tracing::warn!(
                            kind = ?std::mem::discriminant(&other),
                            "unexpected frame post-handshake; closing",
                        );
                        break;
                    }
                }
            }
            AxumWsMessage::Close(_) => break,
            AxumWsMessage::Ping(_) | AxumWsMessage::Pong(_) => continue,
            AxumWsMessage::Text(_) => {
                tracing::warn!("unexpected text frame; closing");
                break;
            }
        }
    }
}
