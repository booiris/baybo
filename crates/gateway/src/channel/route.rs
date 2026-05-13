//! `GET /v1/channel-ws` route — upgrades a channel-auth'd request into
//! a WebSocket, runs the Register handshake, attaches the resulting
//! [`Connection`](aura_channels::Connection) to the registry-owned
//! [`Channel`](aura_channels::Channel) for the requested channel
//! type, and drives the per-connection inbound loop until either side
//! closes.

use std::time::Duration;

use aura_channels::wire::{self, AttachmentKind, Frame, Message as WireMessage, WireAttachment};
use aura_channels::{ChannelKind, IncomingMessage, Message as AgentMessage, MessageRole};
use aura_model::{
    BlobRef, ChannelType, ChatMessage, ContentBlock, MessageMetadata, Role, SessionId, User,
};
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

/// Maximum time to wait for the client's `Register` frame after the WS
/// upgrade completes. Keeps idle connections that never speak from
/// pinning a registry slot.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(5);

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

    // For web-chat connections, take the `TokenHandle` stashed at
    // mint time and move it into the `Sidecar` so the token's
    // lifetime is bound to this WS. When the `Sidecar` drops on WS
    // close, the handle drops with it and the token revokes itself
    // out of `state.tokens`. `None` for non-web auth (TUI / sidecars
    // own their handles elsewhere) and for the rare race where a
    // second WS upgrades with the same token before the first has
    // closed — that second `Sidecar` runs without ownership; the
    // token stays alive as long as either sidecar does.
    let token_handle = match &authed {
        AuthedClient::Web { token, .. } => state
            .web_chat_tokens
            .remove(token)
            .map(|(_, stashed)| stashed.handle),
        _ => None,
    };

    let sidecar = match Sidecar::build(
        channel_type.clone(),
        &state.registry,
        sink,
        std::sync::Arc::clone(&state.blob_store),
        token_handle,
    ) {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(error = %err, %channel_type, "channel-ws build failed");
            // Sink was consumed by build's failure path? No — build
            // owns it, and on error the sink is dropped with the
            // partially-constructed pump. There is nothing left to
            // ack on.
            return;
        }
    };

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

    run_inbound_loop(source, &state, &channel_type, &sidecar).await;

    if kind.is_multiplexed() {
        state.control.unregister(&channel_type);
        state.bot_reconciler.forget(&channel_type);
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
/// `BotStatus`; the WS inbound loop pushes those into `aura-tracing`.
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

async fn run_inbound_loop(
    mut source: SplitStream<WebSocket>,
    state: &WsChannelState,
    channel_type: &ChannelType,
    sidecar: &Sidecar,
) {
    let kind = sidecar.channel.kind();
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
                        // Ship the authoritative pending-approvals
                        // snapshot for this session so the client can
                        // reconcile any locally-cached ApprovalCard
                        // against the queue's truth — covers the case
                        // where an approval was resolved by another tab
                        // while this connection was down (the
                        // `ApprovalResolved` fan-out is fire-and-forget
                        // and not replayed on catch-up).
                        let pending_call_ids =
                            sidecar.channel.pending_approval_call_ids(&session_id);
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
                        // TUI clients get a one-shot history ring from
                        // the gateway-owned vault when they subscribe
                        // so they can rehydrate their scrollback. Any
                        // failure here is surfaced as an empty ring —
                        // a broken history store must not keep the
                        // user from chatting.
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
                        // Catch-up: if the client carried a cursor,
                        // replay the persisted Messages it missed
                        // while disconnected. Sent to this connection
                        // only (not broadcast) so other tabs don't see
                        // the replay storm.
                        if let Some(since) = since_ordinal {
                            replay_catch_up(state, sidecar, channel_type, &session_id, since).await;
                        }
                    }
                    Frame::Unsubscribe { session_id } => {
                        let Some(sub) = sidecar.channel.as_subscribed() else {
                            continue;
                        };
                        sub.unsubscribe(sidecar.connection_id(), &session_id);
                    }
                    Frame::Message(wire_msg) => {
                        let session_id = match resolve_inbound_session(
                            state,
                            sidecar,
                            channel_type,
                            kind,
                            &wire_msg,
                        )
                        .await
                        {
                            Some(sid) => sid,
                            None => continue,
                        };

                        let sender = User {
                            id: wire_msg.user_id.clone(),
                            name: None,
                            channel: channel_type.clone(),
                        };
                        let content =
                            wire_to_content_blocks(wire_msg.content, wire_msg.attachments);
                        let timestamp = Utc::now();
                        let incoming = IncomingMessage {
                            message: AgentMessage {
                                id: uuid::Uuid::new_v4().to_string(),
                                session_id: session_id.clone(),
                                channel: channel_type.clone(),
                                sender,
                                content,
                                timestamp,
                                reply_to: None,
                                metadata: MessageMetadata::default(),
                            },
                            platform_msg_id: wire_msg.platform_msg_id,
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
                        if let Err(e) = state.incoming_tx.send(incoming).await {
                            tracing::error!(error = %e, "router intake closed; tearing down");
                            break;
                        }
                    }
                    Frame::ResolveApproval { call_id, decision } => {
                        // The connection-side message doesn't carry
                        // the session_id; we look it up best-effort
                        // through the broadcast path (the
                        // ApprovalResolved fan-out below targets every
                        // connection subscribed to the call's session,
                        // which the gate already knows). Empty
                        // session_id is acceptable: dispatch uses it
                        // only for the selective-channel reverse
                        // index, and broadcast channels iterate
                        // connections directly.
                        let resolved =
                            sidecar.resolve_approval(&call_id, &SessionId::from(""), decision);
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
                        // The gateway doesn't currently send Ping itself —
                        // a stray Pong is harmless and arrives on the
                        // app-level liveness path, so just accept it.
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
    for (ordinal, msg) in rows {
        let Some(wire) = chat_to_visible_wire_message(channel_type, session_id, ordinal, msg)
        else {
            continue;
        };
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

/// Project a persisted [`ChatMessage`] onto a UI-visible wire Message,
/// or `None` for rows that should never have surfaced as a chat bubble
/// (skill reminders the agent injected as Role::User, tool-call /
/// tool-result rows, raw thinking blocks, system rows). Mirrors the
/// REST transcript path's "what counts as a chat bubble" view so a
/// reconnecting client doesn't see internal turns it wouldn't have
/// seen if it had stayed connected.
fn chat_to_visible_wire_message(
    channel_type: &ChannelType,
    session_id: &SessionId,
    ordinal: i64,
    msg: ChatMessage,
) -> Option<WireMessage> {
    let role = match msg.role {
        Role::User if msg.from_user => MessageRole::User,
        Role::Assistant => MessageRole::Assistant,
        // Role::System rows are the leading prompt — never user-facing.
        // Role::User with from_user=false is an agent-injected reminder.
        // Role::Tool rows are tool results — internal.
        _ => return None,
    };
    let mut text = String::new();
    for block in &msg.content {
        if let ContentBlock::Text(t) = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(t);
        }
    }
    // Empty-text rows that were just tool-use / thinking blocks
    // (assistant turn with no prose) aren't worth surfacing as a
    // bubble — the client would render an empty row.
    if text.is_empty() {
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
        attachments: Vec::new(),
        platform_msg_id: String::new(),
        role,
        ordinal: Some(ordinal),
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
    wire_msg: &aura_channels::wire::Message,
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
    use aura_pairing::CheckOutcome;

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
                 `aura pair approve {code}`\n\
                 Messages won't reach aura until this pairing is approved."
            );
            let notice = Frame::Notice {
                session_id: SessionId::from(""),
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
    use aura_channels::wire::AttachmentKind;

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
}
