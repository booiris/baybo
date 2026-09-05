//! The frame pump: one task per live socket, deliberately OUTSIDE the
//! supervisor loop (the hot path). It routes inbound frames straight to
//! per-session sinks through the shared routing map, answers the gateway's
//! keepalive `Ping` locally, stamps the leg's `last_inbound` proof-of-life
//! cell on every socket yield, and reports lifecycle events
//! (`PumpEnded` / `SubscribeAcked`) to the supervisor tagged with its
//! `leg_id`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message;

use crate::api::FrameSink;
use crate::core::{Frame, ProjectChangeScope, resolve_approval_frame, subscribe_frame};

use super::supervisor::{Msg, OutboundCmd};
use super::{Connection, RoutingMap, SharedDeckSink, SharedListSink, SharedProjectSink};

/// If the socket yields nothing for this long the leg is treated as dead — e.g.
/// frozen across an iOS background round-trip, whose read side never resumes — and
/// the pump exits so the next foreground reconnect re-subscribes. The gateway
/// sends an application keepalive every 20s, so a silent window this long means
/// the socket is gone.
const INBOUND_LIVENESS_TIMEOUT: Duration = Duration::from_secs(45);

/// Everything [`pump`] needs besides the socket: where inbound frames go, the
/// leg's proof-of-life stamp, and the supervisor queue its lifecycle events
/// (`PumpEnded`, `SubscribeAcked`) are reported on, tagged with this pump's
/// `leg_id`.
pub(super) struct PumpCtx {
    pub(super) sinks: RoutingMap,
    pub(super) list_sink: SharedListSink,
    pub(super) deck_sink: SharedDeckSink,
    pub(super) project_sink: SharedProjectSink,
    pub(super) last_inbound: Arc<parking_lot::Mutex<Instant>>,
    pub(super) leg_id: u64,
    pub(super) events: mpsc::UnboundedSender<Msg>,
}

/// Own the socket for the binding's lifetime: fan inbound frames to per-session
/// sinks and seal outbound user messages.
/// The codec hides whether bytes are Noise-sealed (relay) or raw msgpack
/// (direct), so this body is identical for both legs.
///
/// Returning means the socket ended on its own; the tail reports it to the
/// supervisor, whose death transition delivers `on_disconnected`. An ABORTED
/// pump (supervisor teardown, or a discovered corpse) never reaches the tail —
/// by design: the aborter owns the transition, and a late `PumpEnded` from a
/// leg that is no longer current is a no-op there.
pub(super) async fn pump(
    conn: Connection,
    ctx: PumpCtx,
    outbound_rx: mpsc::UnboundedReceiver<OutboundCmd>,
) {
    let leg_id = ctx.leg_id;
    let events = ctx.events.clone();
    run_pump(conn, ctx, outbound_rx).await;
    let _ = events.send(Msg::PumpEnded { leg_id });
}

/// The pump body: send opening frames, then fan inbound frames to the right
/// session sink and seal outbound commands until the leg ends for any reason.
async fn run_pump(
    conn: Connection,
    ctx: PumpCtx,
    mut outbound_rx: mpsc::UnboundedReceiver<OutboundCmd>,
) {
    let Connection {
        ws,
        mut codec,
        user_frame,
    } = conn;
    let (mut sink_ws, mut stream) = ws.split();

    let liveness = tokio::time::sleep(INBOUND_LIVENESS_TIMEOUT);
    tokio::pin!(liveness);

    'session: loop {
        tokio::select! {
            inbound = stream.next() => {
                liveness
                    .as_mut()
                    .reset(tokio::time::Instant::now() + INBOUND_LIVENESS_TIMEOUT);
                // The same signal, published through the leg's proof-of-life
                // cell so the supervisor's ack-timeout judgment can read it
                // (see `Supervisor::ack_timed_out`). Stamped on EVERY socket
                // yield — before the frame is decoded — so locally-answered
                // keepalives count as life.
                *ctx.last_inbound.lock() = Instant::now();
                match inbound {
                    Some(Ok(Message::Binary(bytes))) => {
                        // Relay: a decrypt desync is fatal (Err → break). Direct: an
                        // unknown future variant decodes to Ok(vec![]) and is skipped.
                        let frames = match codec.decode_inbound(&bytes) {
                            Ok(frames) => frames,
                            Err(e) => {
                                log::warn!(
                                    "chat connection ended: inbound frame decode failed (a relay noise desync is unrecoverable): {e}"
                                );
                                break 'session;
                            }
                        };
                        for frame in frames {
                            // Answer the gateway's keepalive locally; never forward it.
                            if matches!(frame, Frame::Ping) {
                                if let Ok(messages) = codec.encode_outbound(&Frame::Pong) {
                                    for bytes in messages {
                                        let _ = sink_ws.send(Message::Binary(bytes)).await;
                                    }
                                }
                                continue;
                            }
                            // The sink is a foreign callback and can't signal a
                            // dropped consumer (unlike the old webview channel);
                            // session lifetime is owned by explicit disconnects.
                            dispatch_inbound_frame(&ctx, frame).await;
                        }
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(frame))) => {
                        match frame {
                            Some(cf) => log::info!(
                                "chat connection ended: socket closed by peer (code={} reason={:?})",
                                u16::from(cf.code),
                                cf.reason
                            ),
                            None => log::info!(
                                "chat connection ended: socket closed by peer (no close frame body)"
                            ),
                        }
                        break 'session;
                    }
                    None => {
                        log::info!(
                            "chat connection ended: socket stream ended"
                        );
                        break 'session;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        log::info!(
                            "chat connection ended: socket read error: {e}"
                        );
                        break 'session;
                    }
                }
            }
            cmd = outbound_rx.recv() => {
                // Both outbound commands build one frame, then share the same
                // encode + chunk + send path (the codec hides Noise vs raw msgpack).
                // `cmd_kind` survives for the log lines below (never the text).
                let (frame, cmd_kind) = match cmd {
                    Some(OutboundCmd::Subscribe { session_id }) => {
                        (
                            subscribe_frame(&session_id),
                            format!("subscribe session={session_id}"),
                        )
                    }
                    Some(OutboundCmd::Send { session_id, text, msg_id, attachments }) => {
                        let frame = user_frame(&session_id, &text, &msg_id, attachments);
                        (frame, format!("send session={session_id} msg_id={msg_id}"))
                    }
                    Some(OutboundCmd::ResolveApproval { call_id, decision }) => {
                        (
                            resolve_approval_frame(&call_id, decision),
                            format!("resolve_approval call_id={call_id}"),
                        )
                    }
                    None => {
                        log::debug!(
                            "chat connection ended: outbound command channel closed"
                        );
                        break 'session;
                    }
                };
                match codec.encode_outbound(&frame) {
                    Ok(messages) => {
                        for bytes in messages {
                            if let Err(e) = sink_ws.send(Message::Binary(bytes)).await {
                                log::warn!(
                                    "chat connection ended: outbound send failed ({cmd_kind}): {e}"
                                );
                                break 'session;
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "outbound frame seal failed; {cmd_kind} dropped, connection stays up: {e}"
                        );
                        continue;
                    }
                }
            }
            _ = &mut liveness => {
                log::info!(
                    "chat connection ended: inbound liveness timeout after {}s (socket presumed dead, e.g. iOS background freeze)",
                    INBOUND_LIVENESS_TIMEOUT.as_secs()
                );
                break 'session;
            }
        }
    }
    let _ = sink_ws.close().await;
}

/// Run `deliver` against a connection-global sink slot, if one is installed.
/// The clone-outside-the-lock shape matters: a foreign (Swift) callback must
/// never run under the slot's mutex.
fn with_global_sink<S: ?Sized>(
    slot: &parking_lot::Mutex<Option<Arc<S>>>,
    deliver: impl FnOnce(&S),
) {
    let sink = slot.lock().clone();
    if let Some(sink) = sink {
        deliver(&sink);
    }
}

fn scope_word(scope: ProjectChangeScope) -> &'static str {
    match scope {
        ProjectChangeScope::Project => "project",
        ProjectChangeScope::Board => "board",
        ProjectChangeScope::Run => "run",
        ProjectChangeScope::Timeline => "timeline",
        // Preserve the frame for older apps; every scope still dirties the board.
        ProjectChangeScope::Unknown => "unknown",
    }
}

pub(super) async fn dispatch_inbound_frame(ctx: &PumpCtx, frame: Frame) {
    let PumpCtx {
        sinks,
        list_sink,
        deck_sink,
        project_sink,
        ..
    } = ctx;
    // Connection-global lanes first. These frames have no per-session routing
    // target (or, for `SessionActivity`, deliberately ignore it): letting them
    // continue would fan them to per-session transcript sinks that can't use
    // them — and reach NOBODY while the user is parked on the chat list or the
    // Deck tab with nothing subscribed, the one moment they matter. A missing
    // sink drops the frame silently by design: the list/deck repaint from
    // their REST snapshots on next open, so nothing is lost.
    //
    // The match yields whether the frame STILL routes per-session: a lane
    // consumes its frame (`false`); the `SessionUpdated` tee and everything
    // unmatched continue (`true`).
    let routes_per_session = match &frame {
        // Activity ping for ANY session — drives chat-list unread/recency.
        Frame::SessionActivity {
            session_id,
            source,
            at,
        } => {
            let source = match source {
                wire::ActivityKind::User => "user",
                wire::ActivityKind::Assistant => "assistant",
            };
            with_global_sink(list_sink, |sink| {
                sink.on_activity(
                    session_id.as_str().to_owned(),
                    source.to_owned(),
                    at.timestamp_millis(),
                )
            });
            false
        }
        // The gateway dropped a session-less broadcast on this connection
        // (bounded queue full) — among them the `SessionActivity` that
        // announces a brand-new session — so the whole list is suspect. A
        // `Gap` that DOES name a session is a transcript concern and keeps
        // the per-session path below.
        Frame::Gap { session_id: None } => {
            with_global_sink(list_sink, |sink| sink.on_list_stale());
            // The same drop can take a `ProjectChanged` with it, and a board
            // has no other way to learn it missed one.
            with_global_sink(project_sink, |sink| sink.on_project_stale());
            false
        }
        // Deck pushes for the connection-global Deck tab.
        Frame::DeckCardData {
            card_id,
            seq,
            payload,
        } => {
            with_global_sink(deck_sink, |sink| {
                sink.on_card_data(card_id.clone(), *seq, payload.clone())
            });
            false
        }
        // Deck structure changed (install, delete, restore, layout, …); the
        // sink answers by refetching `GET /v1/deck`.
        Frame::DeckChanged => {
            with_global_sink(deck_sink, |sink| sink.on_deck_changed());
            false
        }
        Frame::ProjectChanged {
            project_id,
            scope,
            issue_number,
        } => {
            let scope = scope_word(*scope);
            with_global_sink(project_sink, |sink| {
                sink.on_project_changed(project_id.clone(), scope.to_owned(), *issue_number)
            });
            false
        }
        // TEE, not a lane: a `SessionUpdated` patch carrying a freshly-
        // generated title feeds the list sink so a row (subscribed or not)
        // can swap its bold first line live — and STILL routes per-session
        // exactly as before (the transcript webview simply ignores it). Pin /
        // archive / hide patches carry no title and skip the title hop.
        //
        // The approval bit rides the same tee for the same reason, and one it
        // does not share: `Frame::ApprovalRequested` is dispatched only to
        // connections subscribed to that session, so a device sitting on the
        // chat list would otherwise learn nothing about a conversation whose
        // tool call is blocked waiting on it.
        // TEE: this bundle is the gateway's acknowledgement of a `Subscribe`.
        // The supervisor learns of it as a leg-tagged event (this pump's
        // `leg_id` — which is what makes a stale attempt's ack unable to prove
        // a subscription on a different leg) and wakes whoever is parked in
        // `connect` — and it STILL routes per-session, because its payload
        // (turn, work steps, pending approvals, tasks) is what the transcript
        // REPLACEs its view with.
        Frame::SubscribeState { session_id, .. } => {
            let _ = ctx.events.send(Msg::SubscribeAcked {
                leg_id: ctx.leg_id,
                session_id: session_id.as_str().to_owned(),
            });
            true
        }
        Frame::SessionUpdated { session_id, patch } => {
            if let Some(title) = &patch.title {
                with_global_sink(list_sink, |sink| {
                    sink.on_title(session_id.as_str().to_owned(), title.clone())
                });
            }
            if let Some(pending) = patch.approval_pending {
                with_global_sink(list_sink, |sink| {
                    sink.on_approval_pending(session_id.as_str().to_owned(), pending)
                });
            }
            true
        }
        _ => true,
    };
    if routes_per_session {
        route_per_session(sinks, frame).await;
    }
}

/// The per-session tail: a frame that names a session goes to that session's
/// sink (or is dropped); a session-less frame broadcasts to every sink.
async fn route_per_session(sinks: &Mutex<HashMap<String, Arc<dyn FrameSink>>>, frame: Frame) {
    let target = frame
        .routing_session_id()
        .map(|session_id| session_id.as_str().to_owned());
    let json = match serde_json::to_string(&frame) {
        Ok(json) => json,
        Err(e) => {
            log::warn!("inbound frame dropped: JSON serialize failed: {e}");
            return;
        }
    };
    if let Some(session_id) = target {
        let sink = sinks.lock().await.get(&session_id).cloned();
        if let Some(sink) = sink {
            sink.on_frame(json);
        } else {
            log::debug!("inbound frame dropped: no sink for session {session_id}");
        }
    } else {
        let sinks: Vec<Arc<dyn FrameSink>> = sinks.lock().await.values().cloned().collect();
        for sink in sinks {
            sink.on_frame(json.clone());
        }
    }
}
