//! `GET /v1/channel-ws` route — upgrades a channel-auth'd request into
//! a WebSocket, runs the Register handshake, attaches the resulting
//! [`Connection`](baybo_channels::Connection) to the registry-owned
//! [`Channel`](baybo_channels::Channel) for the requested channel
//! type, and drives the per-connection inbound loop until either side
//! closes.

use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message as AxumWsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, State};
use axum::response::IntoResponse;
use axum::routing::get;
use baybo_channels::wire::{
    self, ApprovalCard, AttachmentKind, Frame, MAX_MESSAGE_BATCH_ATTACHMENTS,
    MAX_MESSAGE_BATCH_MESSAGES, MAX_MESSAGE_BATCH_TEXT_BYTES, Message as WireMessage, TaskView,
    TurnSnapshot, WireAttachment,
};
use baybo_channels::{ChannelKind, IncomingMessage, Message as AgentMessage, RouterInbound};
use baybo_model::{BlobRef, ChannelType, ContentBlock, MessageMetadata, SessionId, User};
use chrono::Utc;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};

use super::adapter::Sidecar;
use super::handshake::validate_register;
use super::state::WsChannelState;
use super::work_steps;
use crate::auth::AuthedClient;

/// Maximum time to wait for the client's `Register` frame after the WS
/// upgrade completes. Keeps idle connections that never speak from
/// pinning a registry slot.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard cap for raw channel WS frames. Blob bytes ride HTTP/blob legs; channel
/// frames are control JSON/MessagePack plus blob references, so 256 KiB is enough
/// for legitimate batched text while bounding decode memory.
const MAX_CHANNEL_WS_FRAME_BYTES: usize = 256 * 1024;

pub fn routes() -> Router<WsChannelState> {
    Router::new()
        .route("/channel-ws", get(ws_handler))
        .merge(super::blobs::routes())
}

async fn ws_handler(
    State(state): State<WsChannelState>,
    Extension(authed): Extension<AuthedClient>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.max_message_size(MAX_CHANNEL_WS_FRAME_BYTES)
        .max_frame_size(MAX_CHANNEL_WS_FRAME_BYTES)
        .on_upgrade(move |socket| run_connection(socket, state, authed))
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

    // Resolve the channel (with lazy install fallback for custom
    // out-of-tree sidecars) *before* committing the sink to the
    // build path. On failure the sink is still ours, so we can
    // surface a `RegisterAck { ok: false, reason }` to the peer
    // instead of dropping the socket silently — a silent close
    // leaves channel-sdk-shaped clients in their reconnect ladder
    // with no operator-visible reason for the rejection.
    let channel = match super::adapter::resolve_or_install_channel(&state.registry, &channel_type) {
        Ok(ch) => ch,
        Err(err) => {
            let reason = format!("channel resolve failed: {err}");
            tracing::warn!(error = %err, %channel_type, "channel-ws channel resolve failed");
            send_ack_and_close(&mut sink, false, Some(reason)).await;
            return;
        }
    };

    let sidecar = Sidecar::build(
        channel_type.clone(),
        channel,
        super::adapter::WsFrameSink(sink),
        std::sync::Arc::clone(&state.blob_store),
    );

    // Captured before `sidecar.into_pump()` consumes `sidecar` below, so the
    // disconnect log can pair with this attach by connection_id + lifetime.
    let connection_id = sidecar.connection_id();
    let attached_at = std::time::Instant::now();
    tracing::info!(
        channel_type = %channel_type,
        %connection_id,
        "channel-ws client attached"
    );

    if let Err(e) = sidecar
        .send_frame(Frame::RegisterAck {
            ok: true,
            reason: None,
        })
        .await
    {
        tracing::warn!(error = %e, "failed to send RegisterAck");
        std::mem::drop(sidecar.into_pump());
        return;
    }

    let kind = sidecar.channel.kind();

    // `Multiplexed`-kind channels (telegram, weixin, discord) are
    // the bot-multiplexing sidecars. Register their outbound pump with
    // the control registry so the CLI-driven reconciler can push
    // `StartBot` / `StopBot` frames, push the slash manifest, and
    // stream one `StartBot` for every live bot.
    if kind.is_multiplexed() {
        state
            .control
            .register(channel_type.clone(), sidecar.frame_tx_clone());
        if let Err(e) = sidecar
            .send_frame(Frame::SlashManifest {
                commands: super::slash::manifest(),
            })
            .await
        {
            tracing::warn!(error = %e, %channel_type, "send SlashManifest failed");
        }
        let sent = push_live_bots(&state, &channel_type, &sidecar).await;
        state.bot_reconciler.seed(channel_type.clone(), sent);
    }

    run_inbound_loop(
        super::adapter::WsFrameSource(source),
        &state,
        &channel_type,
        &sidecar,
    )
    .await;

    if kind.is_multiplexed() {
        // Only clean up if this connection still owns the slot — a
        // fast sidecar reconnect can have already swapped the entry
        // to the new pump, in which case blindly evicting here would
        // silently kill control delivery to the live sidecar.
        let our_tx = sidecar.frame_tx_clone();
        if state.control.unregister_if_owned(&channel_type, &our_tx) {
            state.bot_reconciler.forget(&channel_type);
        }
    }
    let _ = sidecar.into_pump().await;

    tracing::info!(
        %channel_type,
        %connection_id,
        duration_ms = attached_at.elapsed().as_millis() as u64,
        "channel-ws client disconnected"
    );
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
/// the vault skips just that bot. The sidecar replies to each with a
/// `BotStatus`; the WS inbound loop pushes those into `baybo-tracing`.
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

pub(super) async fn run_inbound_loop<R: super::adapter::FrameSource>(
    mut source: R,
    state: &WsChannelState,
    channel_type: &ChannelType,
    sidecar: &Sidecar,
) {
    let kind = sidecar.channel.kind();
    while let Some(frame) = source.next_frame().await {
        match frame {
            Frame::Subscribe { session_id } => {
                let Some(sub) = sidecar.channel.as_subscribed() else {
                    tracing::warn!(
                        %channel_type,
                        "Subscribe frame on Multiplexed channel; ignoring (kind auto-wildcards)",
                    );
                    continue;
                };
                // Channel boundary: a session on another channel is invisible
                // here — the WS layer enforces the same scoping the REST layer
                // does (`load_scoped_chat_session`). Web and device both
                // register as `owner`, so a phone subscribes to a web-origin
                // session as an ordinary same-channel subscribe; `tui` and
                // every `Multiplexed` channel stay isolated. A session with no
                // row yet passes (a client-minted id before its first message
                // creates the row); a storage error fails open, which never
                // checked at all.
                match state.session_manager.get(&session_id).await {
                    Ok(Some(session)) if session.channel != *channel_type => {
                        tracing::warn!(
                            %channel_type,
                            %session_id,
                            session_channel = %session.channel,
                            "Subscribe for session outside this connection's channel; rejecting",
                        );
                        if let Err(e) = sidecar
                            .send_frame(Frame::Notice {
                                session_id: session_id.clone(),
                                user_id: String::new(),
                                level: "error".to_string(),
                                text: "session does not belong to this channel".to_string(),
                                transient: false,
                                mid_turn: Some(false),
                                durable_id: None,
                            })
                            .await
                        {
                            tracing::debug!(error = %e, "send subscribe rejection notice failed");
                        }
                        continue;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, %session_id, "subscribe channel check failed; proceeding");
                    }
                }
                let conn_id = sidecar.connection_id();
                if let Err(e) = sub.subscribe(conn_id, session_id.clone()) {
                    tracing::warn!(error = %e, %session_id, "subscribe failed");
                    continue;
                }
                // TUI clients get a one-shot input-history ring
                // from the gateway-owned vault when they
                // subscribe so they can rehydrate scrollback.
                // MUST go first: the TUI client's handshake
                // (`WsClient::connect_tui`) strictly expects
                // `HistorySnapshot` as the next frame after
                // RegisterAck-then-Subscribe and treats anything
                // else as a protocol violation. Any failure is
                // surfaced as an empty ring — a broken history
                // store must not keep the user from chatting.
                if channel_type.as_str() == ChannelType::TUI {
                    let entries = match state.tui_history.load().await {
                        Ok(entries) => entries,
                        Err(e) => {
                            tracing::warn!(error = %format!("{e:#}"), "load tui input history; sending empty snapshot");
                            Vec::new()
                        }
                    };
                    if let Err(e) = sidecar
                        .send_frame(Frame::HistorySnapshot {
                            session_id: session_id.clone(),
                            entries,
                        })
                        .await
                    {
                        tracing::warn!(error = %e, "failed to send HistorySnapshot");
                    }
                }
                // The one atomic state-plane bundle — everything a
                // client needs to know the moment it starts listening.
                // History is NOT replayed here: transcript recovery is
                // the client's REST sync call, always.
                send_subscribe_state(state, sidecar, &session_id).await;
            }
            Frame::Unsubscribe { session_id } => {
                let Some(sub) = sidecar.channel.as_subscribed() else {
                    continue;
                };
                sub.unsubscribe(sidecar.connection_id(), &session_id);
            }
            Frame::Message(wire_msg) => {
                let Some(incoming) =
                    build_inbound_message(state, sidecar, channel_type, kind, wire_msg).await
                else {
                    continue;
                };
                // Echo to every subscriber of this session
                // (including the sender) so multi-tab views
                // converge on identical transcripts through
                // the same render path as agent output.
                // Subscribed-only by design: a multiplexed
                // sidecar (telegram, weixin, …) would receive
                // its own input back and forward it to the
                // upstream platform; getting the
                // `SubscribedView` here makes that
                // structurally impossible (`None` for
                // multiplexed). The dispatch observer
                // installed on the http channel (see
                // `channel/session_pulse.rs`) sees this echo
                // and fans out a throttled
                // `Frame::SessionActivity{User}` so sidebar
                // tabs not subscribed to the session still
                // pick up the unread signal.
                if let Some(sub) = sidecar.channel.as_subscribed() {
                    sub.echo_inbound(incoming.clone());
                }
                if let Err(e) = state
                    .incoming_tx
                    .send(RouterInbound::One(Box::new(incoming)))
                    .await
                {
                    tracing::error!(error = %e, "router intake closed; tearing down");
                    break;
                }
            }
            Frame::Messages { messages } => {
                if let Err(reason) = validate_message_batch(&messages) {
                    tracing::warn!(
                        %channel_type,
                        count = messages.len(),
                        reason = %reason,
                        "dropping invalid channel-ws message batch",
                    );
                    if let Some(first) = messages.first()
                        && let Err(e) = sidecar
                            .send_frame(Frame::Notice {
                                session_id: first.session_id.clone(),
                                user_id: first.user_id.clone(),
                                level: "error".to_string(),
                                text: reason,
                                transient: false,
                                mid_turn: Some(false),
                                durable_id: None,
                            })
                            .await
                    {
                        tracing::debug!(error = %e, "send batch rejection notice failed");
                    }
                    continue;
                }
                // A client batch ("send every queued message at once"):
                // build each row, echo each (same fan-out as a single
                // Message so every tab renders the N rows), then hand the
                // whole group to the router as ONE intake item — the
                // router delivers it to the actor atomically so its
                // coalescing runs them as a single merged turn instead of
                // racing the per-message intake latency. Rows that fail
                // session resolution are skipped, not fatal.
                let mut batch = Vec::with_capacity(messages.len());
                for wire_msg in messages {
                    let Some(incoming) =
                        build_inbound_message(state, sidecar, channel_type, kind, wire_msg).await
                    else {
                        continue;
                    };
                    if let Some(sub) = sidecar.channel.as_subscribed() {
                        sub.echo_inbound(incoming.clone());
                    }
                    batch.push(incoming);
                }
                if batch.is_empty() {
                    continue;
                }
                if let Err(e) = state.incoming_tx.send(RouterInbound::Batch(batch)).await {
                    tracing::error!(error = %e, "router intake closed; tearing down");
                    break;
                }
            }
            Frame::ResolveApproval { call_id, decision } => {
                // The connection-side frame doesn't carry the
                // `session_id`; the queue entry does, and
                // `Sidecar::resolve_approval` reads it off the
                // removed entry so the follow-up broadcast
                // targets the right subscribers (a Subscribed
                // channel's `dispatch_event` keys on
                // `session_id`; an empty placeholder would
                // dispatch to nobody).
                let resolved = sidecar.resolve_approval(&call_id, decision);
                if !resolved {
                    tracing::debug!(
                        call_id = %call_id,
                        "ResolveApproval for unknown call_id; ignored"
                    );
                }
            }
            Frame::HistoryAppend { session_id, entry } => {
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
            Frame::Ping => {
                if let Err(e) = sidecar.send_frame(Frame::Pong).await {
                    tracing::debug!(error = %e, "reply Pong failed");
                }
            }
            Frame::Pong => {
                // Reply to the gateway's own keepalive `Ping` (the
                // outbound pump sends one per `KEEPALIVE_PING_INTERVAL`).
                // No bookkeeping needed — receipt already kept the
                // socket's read side active.
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
}

fn validate_message_batch(messages: &[WireMessage]) -> Result<(), String> {
    if messages.len() > MAX_MESSAGE_BATCH_MESSAGES {
        return Err(format!(
            "message batch exceeds {MAX_MESSAGE_BATCH_MESSAGES} messages",
        ));
    }
    let Some(first) = messages.first() else {
        return Ok(());
    };
    let mut text_bytes = 0usize;
    let mut attachments = 0usize;
    for msg in messages {
        if msg.session_id != first.session_id {
            return Err("message batch must target one session".to_string());
        }
        text_bytes = text_bytes.saturating_add(msg.content.len());
        attachments = attachments.saturating_add(msg.attachments.len());
    }
    if text_bytes > MAX_MESSAGE_BATCH_TEXT_BYTES {
        return Err(format!(
            "message batch text exceeds {MAX_MESSAGE_BATCH_TEXT_BYTES} bytes",
        ));
    }
    if attachments > MAX_MESSAGE_BATCH_ATTACHMENTS {
        return Err(format!(
            "message batch exceeds {MAX_MESSAGE_BATCH_ATTACHMENTS} attachments",
        ));
    }
    Ok(())
}

/// Build and send the one atomic [`Frame::SubscribeState`] bundle to the
/// subscribing connection: turn activity, the in-flight work block's
/// buffered steps, the authoritative pending-approval set (full cards
/// from one atomic queue read — closes the old snapshot's race window
/// where a card could be neither listed nor broadcast), and the planning
/// checklist, stamped with the session's newest persisted ordinal.
/// Component reads degrade independently — a failed read logs and
/// contributes its empty/idle shape rather than suppressing the bundle,
/// and the next live frame or re-subscribe heals it.
async fn send_subscribe_state(state: &WsChannelState, sidecar: &Sidecar, session_id: &SessionId) {
    let as_of_ordinal = match state
        .session_manager
        .latest_session_ordinal(session_id)
        .await
    {
        Ok(ordinal) => ordinal,
        Err(e) => {
            tracing::warn!(error = %e, %session_id, "subscribe-state: newest ordinal read failed");
            None
        }
    };
    let turn = match state.job_lifecycle.active_turn_started_at(session_id).await {
        Ok(started_at) => TurnSnapshot {
            active: started_at.is_some(),
            started_at,
        },
        Err(e) => {
            tracing::warn!(error = %e, %session_id, "subscribe-state: turn read failed");
            TurnSnapshot {
                active: false,
                started_at: None,
            }
        }
    };
    // Do every `.await` FIRST, then snapshot the REPLACE-sets (work steps +
    // pending approvals) and send with NO suspension in between. These two are
    // authoritative replacements on the client, so a live `ApprovalRequested` /
    // `ApprovalResolved` / work frame broadcast during a suspension between the
    // snapshot read and the send could reach the client first and then be
    // overwritten by the stale bundle. Reading them adjacent to the send keeps
    // the window to a non-await span and preserves the "live frame after the
    // bundle wins by frame order" invariant.
    let tasks: Vec<TaskView> = match state.task_store.list(session_id).await {
        Ok(tasks) => tasks.into_iter().map(TaskView::from).collect(),
        Err(e) => {
            tracing::warn!(error = %e, %session_id, "subscribe-state: task read failed");
            Vec::new()
        }
    };
    // The buffer is a superset of everything this connection could have
    // seen live (`note_in_flight` runs before fan-out), so the client
    // REPLACEs its open work block with these steps and lets subsequent
    // live frames append.
    let work_steps = if turn.active {
        work_steps::in_flight_wire_steps(sidecar.channel.in_flight_events(session_id))
    } else {
        Vec::new()
    };
    let pending_approvals: Vec<ApprovalCard> = sidecar
        .channel
        .pending_approvals(session_id)
        .into_iter()
        .map(|req| ApprovalCard {
            call_id: req.call_id,
            tool_call_id: req.tool_call_id,
            user_id: req.user_id,
            tool: req.tool,
            accesses: req.accesses,
            params_preview: req.params_preview,
            description: req.description,
        })
        .collect();
    if let Err(e) = sidecar
        .send_frame(Frame::SubscribeState {
            session_id: session_id.clone(),
            as_of_ordinal,
            turn,
            work_steps,
            pending_approvals,
            tasks,
        })
        .await
    {
        tracing::warn!(error = %e, %session_id, "failed to send SubscribeState");
    }
}

/// Resolve a single inbound wire message to an `IncomingMessage` (session
/// resolution + content-block conversion). `None` when the session can't be
/// resolved (the caller skips it). Shared by the single-`Frame::Message` and
/// the batched-`Frame::Messages` arms so both build identical rows.
async fn build_inbound_message(
    state: &WsChannelState,
    sidecar: &Sidecar,
    channel_type: &ChannelType,
    kind: ChannelKind,
    wire_msg: baybo_channels::wire::Message,
) -> Option<IncomingMessage> {
    let session_id = resolve_inbound_session(state, sidecar, channel_type, kind, &wire_msg).await?;
    // Sender identity is fixed by the channel, not carried in the message.
    // The `owner` channel (web + device, register-validated so a leaked token
    // can't claim another stream) is one `OWNER` sharing one memory/cost
    // namespace, so the wire `user_id` is ignored for it — that field is only
    // a meaningful identity for `Multiplexed` channels, where a bot relays a
    // real external user's id. `tui` likewise carries its own wire id.
    let sender_id = if *channel_type == ChannelType::owner() {
        crate::auth::OWNER_USER_ID.to_owned()
    } else {
        wire_msg.user_id.clone()
    };
    let sender = User {
        id: sender_id,
        name: None,
        channel: channel_type.clone(),
    };
    let content = wire_to_content_blocks(wire_msg.content, wire_msg.attachments);
    Some(IncomingMessage {
        message: AgentMessage {
            id: uuid::Uuid::new_v4().to_string(),
            session_id,
            channel: channel_type.clone(),
            sender,
            content,
            timestamp: Utc::now(),
            reply_to: None,
            metadata: MessageMetadata::default(),
        },
        platform_msg_id: wire_msg.platform_msg_id,
    })
}

/// Resolve the session_id for an inbound Message frame:
/// * `Multiplexed` channel (telegram, weixin): server derives session
///   from `(channel_type, user_id)` via the resolver after pairing.
/// * `Subscribed` channel (tui, http): the connection must already be
///   subscribed to the session_id named on the wire.
async fn resolve_inbound_session(
    state: &WsChannelState,
    sidecar: &Sidecar,
    channel_type: &ChannelType,
    kind: ChannelKind,
    wire_msg: &baybo_channels::wire::Message,
) -> Option<SessionId> {
    match kind {
        ChannelKind::Subscribed => {
            let session_id = SessionId::from(wire_msg.session_id.as_str().trim());
            if session_id.as_str().is_empty() {
                tracing::warn!(
                    %channel_type,
                    "Subscribed channel Message with empty session_id; dropping",
                );
                return None;
            }
            if !sidecar.connection.is_subscribed_to(&session_id) {
                tracing::warn!(
                    %channel_type,
                    %session_id,
                    "Message for session not subscribed by this connection; dropping",
                );
                return None;
            }
            // Idempotent Send: clients supply a stable `platform_msg_id`
            // (typically a UUID generated at composer-submit time) so a
            // retry after a transport blip — point Send / connection
            // drops between send and echo / user mashes the button —
            // doesn't produce a second agent turn for the same message.
            // Subscribed-kind clients have no bot, so `bot_id` is the
            // empty string in the dedup key tuple. Empty `platform_msg_id`
            // continues to opt out: older clients that haven't been
            // updated still get the previous "every send is fresh"
            // behaviour.
            if !state.inbound_dedup.check_and_record(
                channel_type,
                &wire_msg.bot_id,
                &wire_msg.platform_msg_id,
            ) {
                tracing::debug!(
                    %channel_type,
                    %session_id,
                    platform_msg_id = %wire_msg.platform_msg_id,
                    "duplicate inbound on subscribed channel; dropping",
                );
                return None;
            }
            Some(session_id)
        }
        ChannelKind::Multiplexed => {
            // A misbuilt sidecar that always sets session_id would otherwise warn
            // on every inbound message; the condition is permanent, so warn once
            // per connection with the attach identity of the offender.
            if !wire_msg.session_id.as_str().is_empty()
                && sidecar.first_session_id_on_multiplexed_offence()
            {
                tracing::warn!(
                    %channel_type,
                    connection_id = %sidecar.connection_id(),
                    bot_id = %wire_msg.bot_id,
                    "Multiplexed channel sidecar supplied session_id on Message; ignoring (resolver is canonical)",
                );
            }
            if wire_msg.user_id.is_empty() {
                tracing::warn!(
                    %channel_type,
                    "inbound Message with empty user_id; dropping",
                );
                return None;
            }
            if !state.inbound_dedup.check_and_record(
                channel_type,
                &wire_msg.bot_id,
                &wire_msg.platform_msg_id,
            ) {
                tracing::debug!(
                    %channel_type,
                    bot_id = %wire_msg.bot_id,
                    platform_msg_id = %wire_msg.platform_msg_id,
                    "duplicate inbound; dropping",
                );
                return None;
            }
            if !enforce_pairing(
                state,
                sidecar,
                channel_type,
                &wire_msg.bot_id,
                &wire_msg.user_id,
            )
            .await
            {
                return None;
            }
            match super::slash::try_handle(
                &state.session_resolver,
                &wire_msg.content,
                channel_type,
                &wire_msg.user_id,
            )
            .await
            {
                super::slash::SlashOutcome::Handled(reply) => {
                    if let Err(e) = sidecar.send_frame(Frame::Message(reply)).await {
                        tracing::warn!(error = %e, "send slash reply failed");
                    }
                    return None;
                }
                super::slash::SlashOutcome::PassThrough => {}
            }
            match state
                .session_resolver
                .resolve_or_create(channel_type, &wire_msg.user_id)
                .await
            {
                Ok(sid) => Some(sid),
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        %channel_type,
                        user_id = %wire_msg.user_id,
                        "resolve session id for inbound message failed; dropping",
                    );
                    None
                }
            }
        }
    }
}

/// Pairing gate. Returns `true` if the inbound can proceed, `false`
/// if it was dropped (refused or errored). On refusal the pairing
/// code is posted back as a `Frame::Notice` so the sidecar surfaces
/// it through its existing notice routing.
async fn enforce_pairing(
    state: &WsChannelState,
    sidecar: &Sidecar,
    channel_type: &ChannelType,
    bot_id: &str,
    user_id: &str,
) -> bool {
    use baybo_pairing::CheckOutcome;

    match state.pairing.check(channel_type, bot_id, user_id).await {
        Ok(CheckOutcome::Approved) => true,
        Ok(CheckOutcome::Pending { code }) => {
            tracing::warn!(
                %channel_type,
                %bot_id,
                user_id_hash = %super::short_hash(user_id),
                "pairing required; returning code and dropping message",
            );
            let text = format!(
                "🔐 Pairing required. Run:\n\
                 `baybo pair approve {code}`\n\
                 Messages won't reach baybo until this pairing is approved."
            );
            let notice = Frame::Notice {
                session_id: SessionId::from(""),
                user_id: user_id.to_owned(),
                level: "warn".to_owned(),
                text,
                transient: false,
                mid_turn: Some(false),
                durable_id: None,
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
                user_id_hash = %super::short_hash(user_id),
                "pairing check failed; dropping message",
            );
            false
        }
    }
}

/// Translate the wire-level `(content, attachments)` pair into the
/// agent-facing `Vec<ContentBlock>`. Empty text drops the leading
/// `Text` block so a "media-only" message doesn't carry a phantom
/// empty string.
fn wire_to_content_blocks(content: String, attachments: Vec<WireAttachment>) -> Vec<ContentBlock> {
    let mut blocks = Vec::with_capacity(attachments.len() + usize::from(!content.is_empty()));
    if !content.is_empty() {
        blocks.push(ContentBlock::Text(content));
    }
    for att in attachments {
        let blob = BlobRef {
            blob_id: att.blob_id,
        };
        blocks.push(match att.kind {
            AttachmentKind::Image => ContentBlock::Image {
                blob,
                mime_type: att.mime_type,
                filename: att.filename,
            },
            AttachmentKind::Audio => ContentBlock::Audio {
                blob,
                mime_type: att.mime_type,
                filename: att.filename,
                duration_ms: att.duration_ms,
            },
            AttachmentKind::File => ContentBlock::File {
                blob,
                filename: att.filename.unwrap_or_default(),
                mime_type: att.mime_type,
                duration_ms: att.duration_ms,
            },
        });
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_channels::wire::AttachmentKind;

    fn att(kind: AttachmentKind, mime: &str, filename: Option<&str>) -> WireAttachment {
        WireAttachment {
            kind,
            blob_id: format!("sha256:{}", "0".repeat(64)),
            mime_type: mime.into(),
            size: 7,
            filename: filename.map(str::to_owned),
            duration_ms: None,
        }
    }

    #[test]
    fn text_only_yields_single_text_block() {
        let blocks = wire_to_content_blocks("hi".into(), Vec::new());
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], ContentBlock::Text(t) if t == "hi"));
    }

    #[test]
    fn empty_text_with_attachments_skips_leading_text() {
        let blocks = wire_to_content_blocks(
            String::new(),
            vec![att(AttachmentKind::Image, "image/png", None)],
        );
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], ContentBlock::Image { .. }));
    }

    #[test]
    fn text_and_attachments_appear_in_order() {
        let blocks = wire_to_content_blocks(
            "look".into(),
            vec![
                att(AttachmentKind::Audio, "audio/wav", None),
                att(AttachmentKind::File, "application/pdf", Some("a.pdf")),
            ],
        );
        assert_eq!(blocks.len(), 3);
        assert!(matches!(&blocks[0], ContentBlock::Text(t) if t == "look"));
        assert!(matches!(&blocks[1], ContentBlock::Audio { .. }));
        match &blocks[2] {
            ContentBlock::File {
                filename,
                mime_type,
                ..
            } => {
                assert_eq!(filename, "a.pdf");
                assert_eq!(mime_type, "application/pdf");
            }
            other => panic!("expected File block, got {other:?}"),
        }
    }

    #[test]
    fn message_batch_validation_rejects_abuse_shapes() {
        let msg = WireMessage {
            content: "ok".to_string(),
            session_id: SessionId::from("s1"),
            user_id: "u".to_string(),
            channel_type: ChannelType::from("http"),
            bot_id: String::new(),
            attachments: Vec::new(),
            platform_msg_id: String::new(),
            role: baybo_channels::MessageRole::User,
            ordinal: None,
        };

        assert!(validate_message_batch(std::slice::from_ref(&msg)).is_ok());
        assert!(
            validate_message_batch(&vec![msg.clone(); MAX_MESSAGE_BATCH_MESSAGES + 1]).is_err()
        );

        let mut other_session = msg.clone();
        other_session.session_id = SessionId::from("s2");
        assert!(validate_message_batch(&[msg, other_session]).is_err());
    }
}
