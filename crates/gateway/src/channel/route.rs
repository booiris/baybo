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
    self, AttachmentKind, Frame, MAX_MESSAGE_BATCH_ATTACHMENTS, MAX_MESSAGE_BATCH_MESSAGES,
    MAX_MESSAGE_BATCH_TEXT_BYTES, Message as WireMessage, TaskView, WireAttachment,
};
use baybo_channels::{
    ChannelKind, IncomingMessage, Message as AgentMessage, MessageRole, RouterInbound,
};
use baybo_model::{
    BlobRef, ChannelType, ChatMessage, ContentBlock, MessageMetadata, Role, SessionId, User,
};
use chrono::Utc;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};

use super::adapter::Sidecar;
use super::handshake::validate_register;
use super::state::WsChannelState;
use super::work_steps;
use crate::api::admin::chat::{
    DEFAULT_HISTORY_LIMIT, MAX_HISTORY_LIMIT, reconstruct_catchup_work_steps,
};
use crate::auth::AuthedClient;

/// Maximum time to wait for the client's `Register` frame after the WS
/// upgrade completes. Keeps idle connections that never speak from
/// pinning a registry slot.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard cap for raw channel WS frames. Blob bytes ride HTTP/blob legs; channel
/// frames are control JSON/MessagePack plus blob references, so 256 KiB is enough
/// for legitimate batched text while bounding decode memory.
const MAX_CHANNEL_WS_FRAME_BYTES: usize = 256 * 1024;

/// Hard cap on how many persisted Message rows the gateway will replay
/// for a single `Subscribe { since_ordinal }`. A client that fell so
/// far behind that the gap exceeds this is told to refetch via REST
/// (`Frame::Reset`) rather than receive a multi-megabyte WS burst
/// that competes with live traffic.
const MAX_CATCHUP_REPLAY: usize = 200;

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

    tracing::info!(
        channel_type = %channel_type,
        connection_id = %sidecar.connection_id(),
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
            Frame::Subscribe {
                session_id,
                since_ordinal,
            } => {
                let Some(sub) = sidecar.channel.as_subscribed() else {
                    tracing::warn!(
                        %channel_type,
                        "Subscribe frame on Multiplexed channel; ignoring (kind auto-wildcards)",
                    );
                    continue;
                };
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
                // Recover any pending approvals the client
                // missed (or lost to a reload) by replaying
                // the originating `ApprovalRequested` for each.
                // `Frame::ApprovalResolved` is fire-and-forget
                // and the queue itself is the canonical record,
                // so a reconnecting client can render the full
                // prompt only if we resend the request data;
                // shipping just the `call_id` list lets the
                // tool call block until timeout. The follow-up
                // `PendingApprovalsSnapshot` then handles the
                // mirror case — dropping locally-cached cards
                // whose approvals were resolved while this
                // connection was down.
                let pending = sidecar.channel.pending_approvals(&session_id);
                let pending_call_ids: Vec<String> =
                    pending.iter().map(|r| r.call_id.clone()).collect();
                for req in pending {
                    if let Err(e) = sidecar
                        .send_frame(Frame::ApprovalRequested {
                            call_id: req.call_id,
                            session_id: req.session_id,
                            user_id: req.user_id,
                            tool: req.tool,
                            accesses: req.accesses,
                            params_preview: req.params_preview,
                            description: req.description,
                        })
                        .await
                    {
                        tracing::warn!(
                            error = %e,
                            %session_id,
                            "failed to replay pending ApprovalRequested"
                        );
                        break;
                    }
                }
                if let Err(e) = sidecar
                    .send_frame(Frame::PendingApprovalsSnapshot {
                        session_id: session_id.clone(),
                        call_ids: pending_call_ids,
                    })
                    .await
                {
                    tracing::warn!(
                        error = %e,
                        %session_id,
                        "failed to send PendingApprovalsSnapshot"
                    );
                }
                // Catch-up: if the client carried a cursor,
                // replay the persisted Messages it missed
                // while disconnected. Sent to this connection
                // only (not broadcast) so other tabs don't see
                // the replay storm.
                if let Some(since) = since_ordinal {
                    replay_catch_up(state, sidecar, channel_type, &session_id, since).await;
                }
                // Hydrate the durable planning checklist for this
                // connection — reload / reconnect / view-cache eviction
                // all re-subscribe, so this is the single place a client
                // recovers the list without waiting for the next turn.
                // Sent only when non-empty and to this connection only;
                // surfaces without a checklist (TUI) drop the frame.
                match state.task_store.list(&session_id).await {
                    Ok(tasks) if !tasks.is_empty() => {
                        let tasks = tasks.into_iter().map(TaskView::from).collect();
                        if let Err(e) = sidecar
                            .send_frame(Frame::TaskList {
                                session_id: session_id.clone(),
                                user_id: String::new(),
                                tasks,
                            })
                            .await
                        {
                            tracing::warn!(error = %e, %session_id, "failed to send TaskList snapshot");
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, %session_id, "failed to load tasks for snapshot");
                    }
                }
                // Tell this connection whether a turn is in flight
                // right now (and since when), derived from the job
                // store. A new tab / reconnect missed the live
                // `TurnState` broadcasts, and without a definitive
                // answer it can neither run the in-flight work
                // block's elapsed timer nor distinguish "agent
                // still working" from "turn died without a reply".
                // Always sent — `active: false` is load-bearing
                // (it's what authorises the Cancelled indicator).
                match state
                    .job_lifecycle
                    .active_turn_started_at(&session_id)
                    .await
                {
                    Ok(started_at) => {
                        if let Err(e) = sidecar
                            .send_frame(Frame::TurnState {
                                session_id: session_id.clone(),
                                user_id: String::new(),
                                active: started_at.is_some(),
                                started_at,
                            })
                            .await
                        {
                            tracing::warn!(error = %e, %session_id, "failed to send TurnState snapshot");
                        }
                        // Recover the in-flight turn's work block (reasoning /
                        // tool steps) this connection missed while disconnected
                        // — the catch-up above only replays persisted message
                        // bubbles, so a client reconnecting mid-turn (a device
                        // relay leg resuming after backgrounding) would see a
                        // work block with a hole. Only while a turn is
                        // streaming; the buffer lives on this (Subscribed)
                        // channel and is populated even with no subscribers.
                        // `note_in_flight` runs before fan-out, so the buffer is
                        // a superset of everything delivered live to this
                        // connection — the client REPLACES its open block with
                        // it (no double-render of the head). Ordered right after
                        // the active TurnState, which opens the client's block.
                        if started_at.is_some() {
                            let steps = work_steps::in_flight_wire_steps(
                                sidecar.channel.in_flight_events(&session_id),
                            );
                            if !steps.is_empty()
                                && let Err(e) = sidecar
                                    .send_frame(Frame::WorkSnapshot {
                                        session_id: session_id.clone(),
                                        user_id: String::new(),
                                        steps,
                                    })
                                    .await
                            {
                                tracing::warn!(error = %e, %session_id, "failed to send WorkSnapshot");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, %session_id, "failed to derive TurnState snapshot");
                    }
                }
            }
            Frame::Unsubscribe { session_id } => {
                let Some(sub) = sidecar.channel.as_subscribed() else {
                    continue;
                };
                sub.unsubscribe(sidecar.connection_id(), &session_id);
            }
            Frame::FetchHistory {
                session_id,
                before_ordinal,
                limit,
            } => {
                // Backward transcript paging over the live (Noise-sealed) leg —
                // the relay equivalent of REST GET /v1/chat/sessions/:id, for
                // clients (the device relay leg) with no admin REST surface.
                // Subscribed-kind only, and the connection must already be
                // subscribed to the session — parity with the `Message` path
                // below, not true per-device isolation (since `subscribe` itself
                // is unchecked, a device can subscribe to any session first).
                let Some(_sub) = sidecar.channel.as_subscribed() else {
                    continue;
                };
                if !sidecar.connection.is_subscribed_to(&session_id) {
                    tracing::warn!(
                        %channel_type,
                        %session_id,
                        "FetchHistory for session not subscribed by this connection; dropping",
                    );
                    continue;
                }
                send_history_page(
                    state,
                    sidecar,
                    channel_type,
                    &session_id,
                    before_ordinal,
                    limit,
                )
                .await;
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

/// Replay persisted Message rows whose ordinal is strictly greater
/// than the client's `since_ordinal` cursor — sent only to the
/// connection that asked. Filters out rows that aren't user-visible
/// (agent-injected skill reminders, tool calls, tool results,
/// thinking, system messages); the surviving rows carry their
/// absolute `ordinal` on the wire so the client advances its cursor.
///
/// On overflow (gap larger than [`MAX_CATCHUP_REPLAY`]) the gateway
/// sends `Frame::Reset { reason }` instead of partial replay so the
/// client falls back to a paged REST fetch and doesn't end up with
/// an arbitrarily truncated middle slice.
async fn replay_catch_up(
    state: &WsChannelState,
    sidecar: &Sidecar,
    channel_type: &ChannelType,
    session_id: &SessionId,
    since_ordinal: i64,
) {
    let rows = match state
        .session_manager
        .history_since(session_id, since_ordinal, MAX_CATCHUP_REPLAY + 1)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                error = %e,
                %channel_type,
                %session_id,
                since_ordinal,
                "catch-up history fetch failed; client may miss messages",
            );
            return;
        }
    };
    if rows.len() > MAX_CATCHUP_REPLAY {
        tracing::info!(
            %channel_type,
            %session_id,
            since_ordinal,
            cap = MAX_CATCHUP_REPLAY,
            "catch-up slice exceeds cap; sending Reset for REST refetch",
        );
        if let Err(e) = sidecar
            .send_frame(Frame::Reset {
                reason: format!("catch-up gap exceeds {MAX_CATCHUP_REPLAY} rows; refetch via REST"),
            })
            .await
        {
            tracing::debug!(error = %e, "send catch-up Reset failed");
        }
        return;
    }
    // Reconstruct each completed turn's collapsed work block (keyed by its reply
    // ordinal) from the persisted rows, so a client reconnecting AFTER a turn
    // finished recovers the reasoning / tool steps it missed — not just the
    // answer bubble. Sent as a `WorkReplay` right before the turn's reply.
    let work_by_reply = reconstruct_catchup_work_steps(&rows);
    for (ordinal, msg) in rows {
        let Some(wire) = chat_to_visible_wire_message(
            channel_type,
            session_id,
            ordinal,
            msg,
            &*state.blob_store,
        )
        .await
        else {
            continue;
        };
        if let Some(steps) = work_by_reply.get(&ordinal)
            && let Err(e) = sidecar
                .send_frame(Frame::WorkReplay {
                    session_id: session_id.clone(),
                    user_id: String::new(),
                    steps: steps.clone(),
                })
                .await
        {
            tracing::debug!(
                error = %e,
                %channel_type,
                %session_id,
                ordinal,
                "send catch-up WorkReplay failed; continuing with reply",
            );
        }
        if let Err(e) = sidecar.send_frame(Frame::Message(wire)).await {
            tracing::debug!(
                error = %e,
                %channel_type,
                %session_id,
                ordinal,
                "send catch-up Message failed; aborting replay",
            );
            return;
        }
    }
}

/// Serve one **backward** page of `session_id`'s persisted transcript to
/// this connection only, in response to a [`Frame::FetchHistory`] — the
/// Noise-sealed relay equivalent of REST `GET /v1/chat/sessions/:id` for
/// clients (the device relay leg) that have no admin REST surface. Where
/// [`replay_catch_up`] pages *forward* (rows above the cursor, streamed as
/// individual [`Frame::Message`]s) this pages *backward* (rows below
/// `before_ordinal`, or the newest page when `None`) and replies with a
/// single [`Frame::HistoryPage`]. Reuses [`chat_to_visible_wire_message`]
/// so the page carries the exact same UI-visible projection catch-up does.
async fn send_history_page(
    state: &WsChannelState,
    sidecar: &Sidecar,
    channel_type: &ChannelType,
    session_id: &SessionId,
    before_ordinal: Option<i64>,
    limit: Option<u32>,
) {
    // Clamp to the same bounds as the REST endpoint; over-fetch by one so
    // `has_more` is known without a separate COUNT (mirrors `get_session`).
    let want = (limit.unwrap_or(DEFAULT_HISTORY_LIMIT as u32) as usize).clamp(1, MAX_HISTORY_LIMIT);
    let mut rows = match state
        .session_manager
        .history_tail(session_id, before_ordinal, want + 1)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            // A bad/nonexistent session id is a client error, not a reason to
            // tear the connection down — warn and drop the request.
            tracing::warn!(
                error = %e,
                %channel_type,
                %session_id,
                ?before_ordinal,
                "FetchHistory store read failed; dropping request",
            );
            return;
        }
    };
    let has_more = rows.len() > want;
    if has_more {
        // Rows are ascending, so the over-fetch overflow row is the oldest.
        rows.remove(0);
    }
    // Page bounds come from the RAW rows (not the filtered visible set) so the
    // client's paging cursor stays monotonic even across a page whose rows are
    // all internal — matching the REST `oldest`/`newest_ordinal`.
    let oldest_ordinal = rows.first().map(|(o, _, _)| *o);
    let newest_ordinal = rows.last().map(|(o, _, _)| *o);
    let mut messages = Vec::with_capacity(rows.len());
    for (ordinal, _created_at, msg) in rows {
        if let Some(wire) =
            chat_to_visible_wire_message(channel_type, session_id, ordinal, msg, &*state.blob_store)
                .await
        {
            messages.push(wire);
        }
    }
    if let Err(e) = sidecar
        .send_frame(Frame::HistoryPage {
            session_id: session_id.clone(),
            messages,
            oldest_ordinal,
            newest_ordinal,
            has_more,
        })
        .await
    {
        tracing::debug!(
            error = %e,
            %channel_type,
            %session_id,
            "send HistoryPage failed",
        );
    }
}

/// Project a persisted [`ChatMessage`] onto a UI-visible wire Message,
/// or `None` for rows that should never have surfaced as a chat bubble
/// (skill reminders the agent injected as Role::User, tool-call /
/// tool-result rows, raw thinking blocks, system rows). Mirrors the
/// REST transcript path's "what counts as a chat bubble" view so a
/// reconnecting client doesn't see internal turns it wouldn't have
/// seen if it had stayed connected.
async fn chat_to_visible_wire_message(
    channel_type: &ChannelType,
    session_id: &SessionId,
    ordinal: i64,
    msg: ChatMessage,
    blob_store: &dyn baybo_store::BlobStore,
) -> Option<WireMessage> {
    let role = match msg.role {
        Role::User if msg.from_user() => MessageRole::User,
        Role::Assistant => MessageRole::Assistant,
        // Role::System rows are the leading prompt — never user-facing.
        // Role::User with from_user=false is an agent-injected reminder.
        // Role::Tool rows are tool results — internal.
        _ => return None,
    };
    // Intermediate agentic iterations (assistant turns that issued tool
    // calls) carry the model's working narration, which is live-only work
    // progress — not a durable answer bubble. Drop them on catch-up replay
    // so a reconnect agrees with the REST reload path (`api::admin::chat::
    // chat_to_transcript_item`); only the final, tool-call-free reply
    // surfaces.
    if msg.role == Role::Assistant && msg.has_tool_use() {
        return None;
    }
    // Mirror `adapter::split_content`'s text+attachments shape so a
    // row with only Image/Audio/File blocks still surfaces as a wire
    // Message — the REST transcript path keeps such rows visible via
    // `has_attachments`, and dropping them on WS catch-up would let
    // attachment-only messages vanish until a full REST refetch.
    let (text, attachments) = super::adapter::split_content(&msg.content, blob_store).await;
    // A row with neither text nor attachments is structurally an
    // assistant tool-call-only turn or a thinking-only turn; render
    // nothing.
    if text.is_empty() && attachments.is_empty() {
        return None;
    }
    Some(WireMessage {
        content: text,
        session_id: session_id.clone(),
        // Catch-up replay isn't routed by user-id (the connection is
        // already a particular tab); sidecars on Subscribed channels
        // don't consult this field for fan-out.
        user_id: String::new(),
        channel_type: channel_type.clone(),
        bot_id: String::new(),
        attachments,
        platform_msg_id: msg.platform_msg_id().to_string(),
        role,
        ordinal: Some(ordinal),
    })
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
    let sender = User {
        id: wire_msg.user_id.clone(),
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
            if !wire_msg.session_id.as_str().is_empty() {
                tracing::warn!(
                    %channel_type,
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
            },
            AttachmentKind::Audio => ContentBlock::Audio {
                blob,
                mime_type: att.mime_type,
            },
            AttachmentKind::File => ContentBlock::File {
                blob,
                filename: att.filename.unwrap_or_default(),
                mime_type: att.mime_type,
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
