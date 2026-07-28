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
use baybo_store::BlobStore;
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

/// Stem of the name synthesised for a file attachment that arrives
/// without one; the mime's extension is appended.
const NAMELESS_ATTACHMENT_STEM: &str = "attachment";

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
                if let Err(reason) = validate_message(&wire_msg) {
                    tracing::warn!(
                        %channel_type,
                        attachments = wire_msg.attachments.len(),
                        reason = %reason,
                        "dropping invalid channel-ws message",
                    );
                    if let Err(e) = sidecar
                        .send_frame(Frame::Notice {
                            session_id: wire_msg.session_id.clone(),
                            user_id: wire_msg.user_id.clone(),
                            level: "error".to_string(),
                            text: reason,
                            transient: false,
                            mid_turn: Some(false),
                            durable_id: None,
                        })
                        .await
                    {
                        tracing::debug!(error = %e, "send message rejection notice failed");
                    }
                    continue;
                }
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

/// Cap the attachments on a singular [`Frame::Message`]. The batch path
/// caps its aggregate, but a multi-select composer can stage an
/// unbounded count on one message, which never reaches that validator.
fn validate_message(msg: &WireMessage) -> Result<(), String> {
    if msg.attachments.len() > MAX_MESSAGE_BATCH_ATTACHMENTS {
        return Err(format!(
            "message exceeds {MAX_MESSAGE_BATCH_ATTACHMENTS} attachments",
        ));
    }
    Ok(())
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
    let content = wire_to_content_blocks(
        wire_msg.content,
        wire_msg.attachments,
        state.blob_store.as_ref(),
    )
    .await;
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
            // on every inbound message; the condition is a permanent client
            // misbuild, so the first offence process-wide warns with the
            // offender's attach identity and the rest stay at debug.
            if !wire_msg.session_id.as_str().is_empty() {
                static WARNED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::debug!(
                        %channel_type,
                        connection_id = %sidecar.connection_id(),
                        bot_id = %wire_msg.bot_id,
                        "Multiplexed channel sidecar supplied session_id on Message; ignoring (resolver is canonical)",
                    );
                } else {
                    tracing::warn!(
                        %channel_type,
                        connection_id = %sidecar.connection_id(),
                        bot_id = %wire_msg.bot_id,
                        "Multiplexed channel sidecar supplied session_id on Message; ignoring (resolver is canonical)",
                    );
                }
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
async fn wire_to_content_blocks(
    content: String,
    attachments: Vec<WireAttachment>,
    blob_store: &dyn BlobStore,
) -> Vec<ContentBlock> {
    let mut blocks = Vec::with_capacity(attachments.len() + usize::from(!content.is_empty()));
    if !content.is_empty() {
        blocks.push(ContentBlock::Text(content));
    }
    for att in attachments {
        let blob = BlobRef {
            blob_id: att.blob_id,
        };
        blocks.push(match att.kind {
            AttachmentKind::Image => {
                // Server-derived like the audio duration below: a provider
                // tiles an image by its PIXEL grid and bills per tile, so
                // the dimensions are the price and the payload's byte
                // count bounds nothing.
                let (width, height) = probe_image_dimensions(blob_store, &blob.blob_id)
                    .await
                    .unzip();
                ContentBlock::Image {
                    blob,
                    mime_type: att.mime_type,
                    filename: att.filename,
                    width,
                    height,
                }
            }
            AttachmentKind::Audio => {
                // Server-derived, overriding the wire: the context budget
                // charges audio per second, and a client-declared length
                // would let a caller under-price its own upload. The wire
                // value survives only as the display fallback for a
                // container we can't read.
                let duration_ms = probe_audio_duration_ms(blob_store, &blob.blob_id)
                    .await
                    .or(att.duration_ms);
                ContentBlock::Audio {
                    blob,
                    mime_type: att.mime_type,
                    filename: att.filename,
                    duration_ms,
                }
            }
            AttachmentKind::File => {
                let page_count =
                    probe_pdf_page_count(blob_store, &blob.blob_id, &att.mime_type).await;
                // Server-derived like the page count, and for the same
                // reason: a text-like file is delivered as inlined prompt
                // text, so its bytes ARE its price. `att.size` is the
                // client's claim about the same blob and is not used.
                let size_bytes = stat_blob_size(blob_store, &blob.blob_id).await;
                ContentBlock::File {
                    filename: file_display_name(att.filename.as_deref(), &att.mime_type),
                    blob,
                    mime_type: att.mime_type,
                    duration_ms: att.duration_ms,
                    page_count,
                    size_bytes,
                }
            }
        });
    }
    blocks
}

/// Byte length of a stored blob, straight off the metadata row. The wire
/// carries a `size` too, but it is the sender's word for it and the
/// context budget spends the number.
async fn stat_blob_size(blob_store: &dyn BlobStore, blob_id: &str) -> Option<u32> {
    match blob_store.stat(blob_id).await {
        Ok(meta) => u32::try_from(meta.size).ok(),
        Err(e) => {
            tracing::debug!(%blob_id, error = %e, "attachment stat failed; size unknown");
            None
        }
    }
}

/// Read a blob for probing, refusing anything the delivery path would
/// reject on size. `stat` first so an oversize payload is never pulled
/// into memory.
///
/// `max_bytes` is the DELIVERY cap of the arm being probed, not a shared
/// worst case: above it the LLM layer always stubs the block, so a fact
/// recovered from those bytes would be a price charged for something that
/// costs the stub. Reading the wider of the two caps charged an 8-16 MiB
/// PDF its full page price for a block that can never be delivered.
async fn probe_bytes(blob_store: &dyn BlobStore, blob_id: &str, max_bytes: u64) -> Option<Vec<u8>> {
    match blob_store.stat(blob_id).await {
        Ok(meta) if meta.size <= max_bytes => {}
        Ok(meta) => {
            tracing::debug!(%blob_id, size = meta.size, limit = max_bytes, "attachment too large to probe");
            return None;
        }
        Err(e) => {
            tracing::debug!(%blob_id, error = %e, "attachment stat failed; skipping probe");
            return None;
        }
    }
    blob_store.get(blob_id).await.ok()
}

/// Pages in an inbound PDF, probed here because ingest is the one moment
/// the bytes are in hand and the `ContentBlock` that outlives them is all
/// the context budget ever sees. A provider bills a native document per
/// PAGE, and byte count is not a stand-in — measured, real documents run
/// 10 to 4,007 bytes per page.
async fn probe_pdf_page_count(
    blob_store: &dyn BlobStore,
    blob_id: &str,
    mime_type: &str,
) -> Option<u32> {
    if !baybo_llm::delivers_pdf_document(mime_type) {
        return None;
    }
    let bytes = probe_bytes(
        blob_store,
        blob_id,
        baybo_llm::MAX_PDF_DOCUMENT_BYTES as u64,
    )
    .await?;
    // `spawn_blocking`: a whole-payload parse is CPU-bound, and a panic
    // inside it surfaces as a `JoinError` instead of unwinding the reactor.
    tokio::task::spawn_blocking(move || baybo_llm::media_probe::pdf_page_count(&bytes))
        .await
        .ok()
        .flatten()
}

async fn probe_audio_duration_ms(blob_store: &dyn BlobStore, blob_id: &str) -> Option<u32> {
    let bytes = probe_bytes(
        blob_store,
        blob_id,
        baybo_llm::MAX_AUDIO_DOCUMENT_BYTES as u64,
    )
    .await?;
    tokio::task::spawn_blocking(move || baybo_llm::media_probe::audio_duration_ms(&bytes))
        .await
        .ok()
        .flatten()
        .filter(|ms| *ms > 0)
}

/// Pixel dimensions of an inbound image, probed here for the same reason
/// the page count is: a provider bills an image per tile of its pixel
/// grid, and the `ContentBlock` that outlives the bytes is all the
/// context budget ever sees.
async fn probe_image_dimensions(blob_store: &dyn BlobStore, blob_id: &str) -> Option<(u32, u32)> {
    let bytes = probe_bytes(
        blob_store,
        blob_id,
        baybo_llm::MAX_IMAGE_DOCUMENT_BYTES as u64,
    )
    .await?;
    tokio::task::spawn_blocking(move || baybo_llm::media_probe::image_dimensions(&bytes))
        .await
        .ok()
        .flatten()
}

/// A `File` block's filename is the only label the transcript card has,
/// and the clients fall back on an *absent* name, not on an empty one —
/// so a nameless or blank inbound attachment is named here rather than
/// stored as `""` and rendered as a titleless card.
fn file_display_name(filename: Option<&str>, mime_type: &str) -> String {
    match filename.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => name.to_owned(),
        None => format!("{NAMELESS_ATTACHMENT_STEM}{}", display_extension(mime_type)),
    }
}

/// Extension for the synthesised name above, or `""` for a MIME we can't
/// label.
///
/// **Deliberately its own table, not `SqliteBlobStore`'s private
/// `mime_extension`.** That one is the frozen on-disk blob layout, where
/// adding an entry renames already-stored payloads; this one only
/// decorates a label a user reads, so it grows freely as new file pickers
/// show up. Merging them would turn every cosmetic addition into a data
/// migration — the duplication is the point.
fn display_extension(mime_type: &str) -> &'static str {
    let bare = mime_type
        .split(';')
        .next()
        .unwrap_or(mime_type)
        .trim()
        .to_ascii_lowercase();
    match bare.as_str() {
        "image/jpeg" | "image/jpg" => ".jpg",
        "image/png" => ".png",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/heic" => ".heic",
        "image/heif" => ".heif",
        "image/svg+xml" => ".svg",
        "image/bmp" => ".bmp",
        "audio/ogg" | "audio/opus" => ".ogg",
        "audio/mpeg" => ".mp3",
        "audio/mp4" | "audio/m4a" => ".m4a",
        "audio/wav" | "audio/x-wav" => ".wav",
        "audio/flac" => ".flac",
        "audio/silk" => ".silk",
        "video/mp4" => ".mp4",
        "video/webm" => ".webm",
        "video/quicktime" => ".mov",
        "video/x-matroska" => ".mkv",
        "application/pdf" => ".pdf",
        "application/zip" => ".zip",
        "application/json" => ".json",
        "application/x-tar" => ".tar",
        "application/gzip" | "application/x-gzip" => ".gz",
        "application/x-7z-compressed" => ".7z",
        "application/vnd.rar" | "application/x-rar-compressed" => ".rar",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => ".docx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => ".xlsx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => ".pptx",
        "application/octet-stream" => ".bin",
        "application/yaml" | "application/x-yaml" | "text/yaml" | "text/x-yaml" => ".yaml",
        "application/toml" | "text/x-toml" => ".toml",
        "text/plain" => ".txt",
        "text/html" => ".html",
        "text/csv" => ".csv",
        "text/markdown" => ".md",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_channels::wire::AttachmentKind;
    use baybo_llm::media_probe::fixture;
    use baybo_storage::test_support::MemoryBlobStore;

    /// The probes `stat` before they read, so a blob that was never
    /// uploaded simply yields no measurement — which is what these
    /// naming / ordering cases want.
    fn no_blobs() -> MemoryBlobStore {
        MemoryBlobStore::new()
    }

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

    #[tokio::test]
    async fn text_only_yields_single_text_block() {
        let blocks = wire_to_content_blocks("hi".into(), Vec::new(), &no_blobs()).await;
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], ContentBlock::Text(t) if t == "hi"));
    }

    async fn stored(store: &MemoryBlobStore, mime: &str, bytes: &[u8]) -> WireAttachment {
        let blob = store.put(bytes, mime, None).await.expect("put");
        WireAttachment {
            kind: if mime.starts_with("audio/") {
                AttachmentKind::Audio
            } else {
                AttachmentKind::File
            },
            blob_id: blob.blob_id,
            mime_type: mime.into(),
            size: bytes.len() as u32,
            filename: Some("upload".into()),
            duration_ms: None,
        }
    }

    /// Ingest is the one moment the bytes are in hand, and the block that
    /// outlives them is all the context budget ever sees. A PDF's page
    /// count is what a provider bills, so it is measured here rather than
    /// guessed downstream from a byte count that spans 10 to 4,007 bytes
    /// per page.
    #[tokio::test]
    async fn an_inbound_pdf_carries_its_probed_page_count() {
        let store = no_blobs();
        for pages in [1usize, 13, 200] {
            let att = stored(
                &store,
                "application/pdf",
                &baybo_llm::media_probe::fixture::object_stream(pages),
            )
            .await;
            let blocks = wire_to_content_blocks(String::new(), vec![att], &store).await;
            match &blocks[0] {
                ContentBlock::File { page_count, .. } => {
                    assert_eq!(*page_count, Some(pages as u32))
                }
                other => panic!("expected File, got {other:?}"),
            }
        }
    }

    /// A text-like file is delivered as inlined prompt text, so its byte
    /// count is its price. Without it every such block was charged the
    /// full 16 KiB delivery cap — 17,529 tokens for a 400-byte `.md`, and
    /// six of them tripped compaction on a 128k window on their own. The
    /// wire's own `size` is the sender's claim and is not what is stored.
    #[tokio::test]
    async fn an_inbound_file_carries_its_stored_byte_size() {
        let store = no_blobs();
        let body = b"# notes\nnothing much here\n";
        let mut att = stored(&store, "text/markdown", body).await;
        att.size = 999_999;
        let blocks = wire_to_content_blocks(String::new(), vec![att], &store).await;
        match &blocks[0] {
            ContentBlock::File { size_bytes, .. } => {
                assert_eq!(*size_bytes, Some(body.len() as u32))
            }
            other => panic!("expected File, got {other:?}"),
        }
    }

    /// A blob that was never uploaded cannot be stat'd, and the budget
    /// then falls back to the delivery cap rather than to a guess.
    #[tokio::test]
    async fn an_unstorable_file_carries_no_byte_size() {
        let blocks = wire_to_content_blocks(
            String::new(),
            vec![att(AttachmentKind::File, "text/markdown", Some("a.md"))],
            &no_blobs(),
        )
        .await;
        match &blocks[0] {
            ContentBlock::File { size_bytes, .. } => assert_eq!(*size_bytes, None),
            other => panic!("expected File, got {other:?}"),
        }
    }

    /// A provider tiles an image by its PIXEL grid, so the dimensions are
    /// the price. Every case here is a fraction of a megabyte, which is
    /// why the payload cap the image arm already had bounded nothing: a
    /// 12000x9000 render costs 49,536 tokens against a 9,288 ceiling.
    #[tokio::test]
    async fn an_inbound_image_carries_its_probed_dimensions() {
        let store = no_blobs();
        for (w, h) in [(3024u32, 4032u32), (12000, 9000)] {
            let mut att = stored(&store, "image/png", &fixture::png(w, h)).await;
            att.kind = AttachmentKind::Image;
            let blocks = wire_to_content_blocks(String::new(), vec![att], &store).await;
            match &blocks[0] {
                ContentBlock::Image { width, height, .. } => {
                    assert_eq!((*width, *height), (Some(w), Some(h)))
                }
                other => panic!("expected Image, got {other:?}"),
            }
        }
    }

    /// Nothing to measure — a vector image, or a blob that was never
    /// uploaded — leaves the fields unset, and the budget then charges the
    /// delivery cap rather than a guess.
    #[tokio::test]
    async fn an_unmeasurable_image_carries_no_dimensions() {
        let store = no_blobs();
        let mut svg = stored(&store, "image/svg+xml", br#"<svg width="9"/>"#).await;
        svg.kind = AttachmentKind::Image;
        let missing = att(AttachmentKind::Image, "image/png", Some("gone.png"));
        for att in [svg, missing] {
            let blocks = wire_to_content_blocks(String::new(), vec![att], &store).await;
            match &blocks[0] {
                ContentBlock::Image { width, height, .. } => {
                    assert_eq!((*width, *height), (None, None))
                }
                other => panic!("expected Image, got {other:?}"),
            }
        }
    }

    /// A payload the delivery path will always stub is not worth probing:
    /// the number would be a price charged for a block that costs the
    /// text stub. The probe budget is each arm's OWN delivery cap, not the
    /// wider of the two — an 8-16 MiB PDF was charged its full page price
    /// for a block that can never be delivered.
    #[tokio::test]
    async fn a_payload_over_its_own_delivery_cap_is_not_probed() {
        // Padded in FRONT of the header, which every lopdf entry point
        // skips, so the document still parses at any size.
        let padded = |bytes: usize| {
            let doc = fixture::classic(3);
            let mut out = vec![b'\n'; bytes - doc.len()];
            out.extend_from_slice(&doc);
            out
        };
        let store = no_blobs();
        let over = padded(baybo_llm::MAX_PDF_DOCUMENT_BYTES + 1);
        assert!(
            over.len() < baybo_llm::MAX_AUDIO_DOCUMENT_BYTES,
            "the shared 16 MiB budget would still have probed this"
        );
        assert_eq!(baybo_llm::media_probe::pdf_page_count(&over), Some(3));

        let att = stored(&store, "application/pdf", &over).await;
        let blocks = wire_to_content_blocks(String::new(), vec![att], &store).await;
        match &blocks[0] {
            ContentBlock::File { page_count, .. } => assert_eq!(*page_count, None),
            other => panic!("expected File, got {other:?}"),
        }

        // …and one exactly at the cap still is.
        let att = stored(
            &store,
            "application/pdf",
            &padded(baybo_llm::MAX_PDF_DOCUMENT_BYTES),
        )
        .await;
        let blocks = wire_to_content_blocks(String::new(), vec![att], &store).await;
        match &blocks[0] {
            ContentBlock::File { page_count, .. } => assert_eq!(*page_count, Some(3)),
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_non_pdf_file_carries_no_page_count() {
        let store = no_blobs();
        let att = stored(&store, "application/zip", b"PK\x03\x04").await;
        let blocks = wire_to_content_blocks(String::new(), vec![att], &store).await;
        match &blocks[0] {
            ContentBlock::File { page_count, .. } => assert_eq!(*page_count, None),
            other => panic!("expected File, got {other:?}"),
        }
    }

    /// The wire's `duration_ms` is a client claim and the budget charges
    /// audio per second, so ingest overrides it with the bytes' own
    /// answer. Here the client under-claims by 136 seconds.
    #[tokio::test]
    async fn an_inbound_voice_note_is_measured_not_taken_at_its_word() {
        let store = no_blobs();
        let mut att = stored(
            &store,
            "audio/wav",
            &baybo_llm::media_probe::fixture::wav(137),
        )
        .await;
        att.duration_ms = Some(1_000);
        let blocks = wire_to_content_blocks(String::new(), vec![att], &store).await;
        match &blocks[0] {
            ContentBlock::Audio { duration_ms, .. } => assert_eq!(*duration_ms, Some(137_000)),
            other => panic!("expected Audio, got {other:?}"),
        }
    }

    /// An unreadable container keeps the client's value for the card's
    /// label; the budget treats a duration it cannot vouch for as unknown
    /// anyway, because delivery re-probes before sending.
    #[tokio::test]
    async fn an_unreadable_container_falls_back_to_the_wire_value() {
        let store = no_blobs();
        let mut att = stored(&store, "audio/ogg", b"not really ogg").await;
        att.duration_ms = Some(4_200);
        let blocks = wire_to_content_blocks(String::new(), vec![att], &store).await;
        match &blocks[0] {
            ContentBlock::Audio { duration_ms, .. } => assert_eq!(*duration_ms, Some(4_200)),
            other => panic!("expected Audio, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_text_with_attachments_skips_leading_text() {
        let blocks = wire_to_content_blocks(
            String::new(),
            vec![att(AttachmentKind::Image, "image/png", None)],
            &no_blobs(),
        )
        .await;
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], ContentBlock::Image { .. }));
    }

    #[tokio::test]
    async fn text_and_attachments_appear_in_order() {
        let blocks = wire_to_content_blocks(
            "look".into(),
            vec![
                att(AttachmentKind::Audio, "audio/wav", None),
                att(AttachmentKind::File, "application/pdf", Some("a.pdf")),
            ],
            &no_blobs(),
        )
        .await;
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

    #[tokio::test]
    async fn nameless_file_attachment_is_named_from_its_mime() {
        for wire_name in [None, Some(""), Some("   ")] {
            let blocks = wire_to_content_blocks(
                String::new(),
                vec![att(AttachmentKind::File, "application/pdf", wire_name)],
                &no_blobs(),
            )
            .await;
            match &blocks[0] {
                ContentBlock::File { filename, .. } => assert_eq!(filename, "attachment.pdf"),
                other => panic!("expected File block, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn display_table_names_arbitrary_file_picks() {
        // These MIMEs map to `""` in the frozen on-disk table on
        // purpose; the user-facing name is where they get a suffix.
        for (mime, expected) in [
            ("application/octet-stream", "attachment.bin"),
            (
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "attachment.docx",
            ),
            (
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "attachment.xlsx",
            ),
            (
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                "attachment.pptx",
            ),
            ("application/yaml", "attachment.yaml"),
            ("text/x-yaml", "attachment.yaml"),
            ("application/toml", "attachment.toml"),
            ("text/x-toml", "attachment.toml"),
            ("application/x-7z-compressed", "attachment.7z"),
            ("application/vnd.rar", "attachment.rar"),
            ("application/x-rar-compressed", "attachment.rar"),
            ("application/json", "attachment.json"),
            ("APPLICATION/JSON; charset=utf-8", "attachment.json"),
        ] {
            let blocks = wire_to_content_blocks(
                String::new(),
                vec![att(AttachmentKind::File, mime, None)],
                &no_blobs(),
            )
            .await;
            match &blocks[0] {
                ContentBlock::File { filename, .. } => assert_eq!(filename, expected, "{mime}"),
                other => panic!("expected File block, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn nameless_file_of_unknown_mime_keeps_a_bare_stem() {
        let blocks = wire_to_content_blocks(
            String::new(),
            vec![att(
                AttachmentKind::File,
                "application/x-not-known",
                Some(""),
            )],
            &no_blobs(),
        )
        .await;
        match &blocks[0] {
            ContentBlock::File { filename, .. } => assert_eq!(filename, "attachment"),
            other => panic!("expected File block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn real_file_name_passes_through_untouched() {
        let blocks = wire_to_content_blocks(
            String::new(),
            vec![att(
                AttachmentKind::File,
                "application/octet-stream",
                Some("quarterly.numbers"),
            )],
            &no_blobs(),
        )
        .await;
        match &blocks[0] {
            ContentBlock::File { filename, .. } => assert_eq!(filename, "quarterly.numbers"),
            other => panic!("expected File block, got {other:?}"),
        }
    }

    #[test]
    fn singular_message_rejects_over_cap_attachments() {
        let mut msg = WireMessage {
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
        msg.attachments = vec![
            att(AttachmentKind::File, "application/pdf", Some("a.pdf"));
            MAX_MESSAGE_BATCH_ATTACHMENTS
        ];
        assert!(validate_message(&msg).is_ok());

        msg.attachments
            .push(att(AttachmentKind::File, "application/pdf", Some("a.pdf")));
        assert!(validate_message(&msg).is_err());
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
