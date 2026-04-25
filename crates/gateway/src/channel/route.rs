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
use crate::auth::AuthedClient;
use crate::log_buffer::LogLevel;

/// Defensive cap on the size of a forwarded sidecar log line. The SDK
/// enforces the same limit on the sender side; this is the belt-and-
/// braces so a malicious sidecar can't flood the LogBuffer with
/// arbitrarily large lines.
const SIDECAR_LOG_MAX_BYTES: usize = 1024;

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

    let outcome = match receive_register(&mut source).await {
        Ok(frame) => match validate_register(frame, &authed, &state.tokens) {
            Ok(outcome) => outcome,
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

    let channel_type = outcome.channel_type;
    let session_id = outcome.session_id;

    let sidecar = Sidecar::build(channel_type.clone(), session_id.clone(), sink);

    if let Err(err) = state
        .registry
        .register(std::sync::Arc::clone(&sidecar.channel))
    {
        let reason = match &err {
            ChannelError::DuplicateChannel(ct) => format!("channel '{ct}' already registered"),
            ChannelError::DuplicateSessionClient(sid) => {
                format!("another client is already attached to session '{sid}'")
            }
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

    tracing::info!(
        channel_type = %channel_type,
        session_id = ?session_id,
        "channel-ws client registered"
    );

    if let Err(e) = sidecar
        .send_frame(Frame::RegisterAck {
            ok: true,
            reason: None,
        })
        .await
    {
        tracing::warn!(error = %e, "failed to send RegisterAck");
        unregister_best_effort(&state, &channel_type, session_id.as_deref());
        let _ = sidecar.into_pump().await;
        return;
    }

    // Session-scoped TUI clients get a one-shot history ring from the
    // gateway-owned vault so they can rehydrate their scrollback without
    // opening the vault themselves. Sidecars (session_id = None) never
    // receive this frame. Any failure here is surfaced as an empty ring
    // — a broken history store must not keep the user from chatting.
    if channel_type.as_str() == ChannelType::TUI
        && let Some(sid) = session_id.as_deref()
    {
        let entries = match state.tui_history.load().await {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "load tui input history; sending empty snapshot");
                Vec::new()
            }
        };
        if let Err(e) = sidecar
            .send_frame(Frame::HistorySnapshot {
                session_id: sid.to_owned(),
                entries,
            })
            .await
        {
            tracing::warn!(error = %e, "failed to send HistorySnapshot");
            unregister_best_effort(&state, &channel_type, session_id.as_deref());
            let _ = sidecar.into_pump().await;
            return;
        }
    }

    // Sidecar-flavored clients (not session-scoped TUIs) are eligible
    // for hot bot provisioning: register their outbound pump with the
    // control registry so the CLI-driven reconciler can push
    // `StartBot` / `StopBot` frames, then stream one `StartBot` for
    // every live bot in the `channel_bots` table so the sidecar picks
    // up the current roster without waiting a reconcile tick. Seed
    // the reconciler so it doesn't double-send on its first tick.
    if session_id.is_none() {
        state
            .control
            .register(channel_type.clone(), sidecar.frame_tx_clone());
        let sent = push_live_bots(&state, &channel_type, &sidecar).await;
        state.bot_reconciler.seed(channel_type.clone(), sent);
    }

    run_inbound_loop(source, &state, &channel_type, &sidecar).await;

    if session_id.is_none() {
        state.control.unregister(&channel_type);
        state.bot_reconciler.forget(&channel_type);
    }
    unregister_best_effort(&state, &channel_type, session_id.as_deref());
    let _ = sidecar.into_pump().await;
    tracing::info!(
        %channel_type,
        session_id = ?session_id,
        "channel-ws client disconnected"
    );
}

fn unregister_best_effort(
    state: &WsChannelState,
    channel_type: &ChannelType,
    session_id: Option<&str>,
) {
    let result = match session_id {
        Some(sid) => state.registry.unregister_session(sid),
        None => state.registry.unregister_sidecar(channel_type.clone()),
    };
    if let Err(e) = result {
        tracing::debug!(error = %e, %channel_type, session_id = ?session_id, "unregister after ws drop");
    }
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

/// Stream `StartBot` for every live bot in the `channel_bots` table
/// to the freshly-connected sidecar. A failure fetching the token from
/// the vault skips just that bot (logged with its id) so one bad row
/// doesn't keep the rest of the roster from coming online. The
/// sidecar replies to each with a `BotStatus`; the WS inbound loop
/// pushes those into `aura-tracing` for operator visibility.
async fn push_live_bots(
    state: &WsChannelState,
    channel_type: &ChannelType,
    sidecar: &Sidecar,
) -> Vec<String> {
    let mut sent = Vec::new();
    let bots = match state.channel_bot_store.list_live(channel_type).await {
        Ok(list) => list,
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                %channel_type,
                "list live bots failed; no StartBot frames sent",
            );
            return sent;
        }
    };
    for row in bots {
        let secret_name = bot_secret_name(channel_type, &row.bot_id);
        let token = match state.secret_vault.get_secret(&secret_name).await {
            Ok(Some(v)) => String::from_utf8_lossy(v.as_bytes()).into_owned(),
            Ok(None) => {
                tracing::warn!(
                    %channel_type,
                    bot_id = %row.bot_id,
                    secret = %secret_name,
                    "bot metadata exists but vault secret missing; skipping StartBot",
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    %channel_type,
                    bot_id = %row.bot_id,
                    "decrypt bot token failed; skipping StartBot",
                );
                continue;
            }
        };
        if let Err(e) = sidecar
            .send_frame(Frame::StartBot {
                bot_id: row.bot_id.clone(),
                token,
            })
            .await
        {
            tracing::warn!(
                error = %e,
                %channel_type,
                bot_id = %row.bot_id,
                "push StartBot failed; WS pump may be closing",
            );
            return sent;
        }
        sent.push(row.bot_id);
    }
    sent
}

/// Deterministic vault key for a bot token. Kept in one place so the
/// admin API and the route layer agree on where the token lives.
pub(crate) fn bot_secret_name(channel_type: &ChannelType, bot_id: &str) -> String {
    format!("channel.{}.bot.{}.token", channel_type.as_str(), bot_id)
}

/// Push one forwarded sidecar log line into the shared `LogBuffer`.
///
/// Attribution is scoped to the sidecar's `ChannelType`, with an
/// optional sidecar-supplied `target` suffix. Unknown level strings
/// degrade to `info` so a typo on the sidecar side never drops the
/// record. The caller has already accepted the frame past the WS
/// decode; any truncation we do here is purely a size safety net —
/// the TS SDK enforces the same 1 KB cap on the sender side.
fn push_sidecar_log(
    state: &WsChannelState,
    channel_type: &ChannelType,
    level: &str,
    text: String,
    target: Option<&str>,
) {
    let level = LogLevel::parse(level).unwrap_or(LogLevel::Info);
    let message = if text.len() > SIDECAR_LOG_MAX_BYTES {
        // `String::truncate` panics on a non-char-boundary cut; scan
        // back to the nearest boundary at or below the cap.
        let mut cut = SIDECAR_LOG_MAX_BYTES;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        let mut truncated = text;
        truncated.truncate(cut);
        truncated.push_str(" [...truncated]");
        truncated
    } else {
        text
    };
    let attributed = match target {
        Some(t) if !t.is_empty() => format!("sidecar::{channel_type}::{t}"),
        _ => format!("sidecar::{channel_type}"),
    };
    state.log_buffer.push_external(level, attributed, message);
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
                        // Sidecars that don't mint their own UUIDs (the
                        // Telegram channel, future Discord / Slack, …)
                        // send `session_id = ""` and rely on the gateway
                        // to allocate one keyed on (channel_type,
                        // user_id). The TUI always fills session_id in
                        // itself so it skips this branch (and the
                        // pairing gate below).
                        let session_id = if wire_msg.session_id.is_empty() {
                            if wire_msg.user_id.is_empty() {
                                tracing::warn!(
                                    %channel_type,
                                    "inbound Message with empty session_id AND user_id; dropping",
                                );
                                continue;
                            }
                            // Pairing gate: unknown / expired-pending
                            // triples get a short code back via Notice
                            // and the message is dropped before any
                            // session is created.
                            if !enforce_pairing(
                                state,
                                sidecar,
                                channel_type,
                                &wire_msg.bot_id,
                                &wire_msg.user_id,
                            )
                            .await
                            {
                                continue;
                            }
                            match state
                                .session_resolver
                                .resolve_or_create(channel_type, &wire_msg.user_id)
                                .await
                            {
                                Ok(sid) => sid,
                                Err(e) => {
                                    tracing::error!(
                                        error = %e,
                                        %channel_type,
                                        user_id = %wire_msg.user_id,
                                        "resolve session id for inbound message failed; dropping",
                                    );
                                    continue;
                                }
                            }
                        } else {
                            wire_msg.session_id
                        };

                        let sender = User {
                            id: wire_msg.user_id.clone(),
                            name: None,
                            channel: channel_type.clone(),
                        };
                        let incoming = IncomingMessage {
                            message: AgentMessage {
                                id: uuid::Uuid::new_v4().to_string(),
                                session_id,
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
                    Frame::HistoryAppend { session_id, entry } => {
                        // Fire-and-forget: zsh-style history shouldn't
                        // block the submit path. Rejecting non-TUI
                        // channel types keeps sidecars from sneaking
                        // writes into the TUI's vault key.
                        if channel_type.as_str() != ChannelType::TUI {
                            tracing::warn!(
                                %channel_type,
                                "HistoryAppend from non-tui channel type; dropping"
                            );
                            continue;
                        }
                        if let Err(e) = state.tui_history.append(&entry).await {
                            tracing::warn!(
                                error = %format!("{e:#}"),
                                %session_id,
                                "append tui input history"
                            );
                        }
                    }
                    Frame::SidecarLog {
                        level,
                        text,
                        target,
                    } => {
                        push_sidecar_log(state, channel_type, &level, text, target.as_deref());
                    }
                    Frame::BotStatus {
                        bot_id,
                        ok,
                        message,
                    } => {
                        if ok {
                            tracing::info!(
                                %channel_type,
                                %bot_id,
                                detail = message.as_deref().unwrap_or(""),
                                "sidecar ack: bot ready",
                            );
                        } else {
                            tracing::warn!(
                                %channel_type,
                                %bot_id,
                                detail = message.as_deref().unwrap_or(""),
                                "sidecar ack: bot failed",
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

/// Pairing gate. Returns `true` if the inbound can proceed, `false`
/// if it was dropped (refused or errored). On refusal the pairing
/// code is posted back as a `Frame::Notice` with `level = "warn"` so
/// the sidecar surfaces it to the end-user through its existing
/// notice routing.
async fn enforce_pairing(
    state: &WsChannelState,
    sidecar: &Sidecar,
    channel_type: &ChannelType,
    bot_id: &str,
    user_id: &str,
) -> bool {
    use aura_pairing::CheckOutcome;

    match state.pairing.check(channel_type, bot_id, user_id).await {
        Ok(CheckOutcome::Approved) => true,
        Ok(CheckOutcome::Pending { code }) => {
            tracing::warn!(
                %channel_type,
                %bot_id,
                user_id_hash = %short_hash(user_id),
                "pairing required; returning code and dropping message",
            );
            let text = format!(
                "🔐 Pairing required. Run:\n\
                 `aura pair approve {code}`\n\
                 Messages won't reach aura until this pairing is approved."
            );
            let notice = Frame::Notice {
                session_id: String::new(),
                user_id: user_id.to_owned(),
                level: "warn".to_owned(),
                text,
            };
            if let Err(e) = sidecar.send_frame(notice).await {
                tracing::debug!(error = %e, "send pairing notice failed");
            }
            false
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                %channel_type,
                %bot_id,
                user_id_hash = %short_hash(user_id),
                "pairing check failed; dropping message",
            );
            false
        }
    }
}

/// Deterministic short hash of an identifier for log attribution.
/// Four hex chars is enough to distinguish concurrent pendings in a
/// tracing log without leaking the raw id.
fn short_hash(raw: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    raw.hash(&mut h);
    format!("{:04x}", (h.finish() & 0xFFFF) as u16)
}
