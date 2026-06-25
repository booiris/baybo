//! The content-session command: dial the gateway's `/v1/device/content` WS,
//! run the Noise IK handshake, and pump `Frame`s between the webview and the
//! gateway over the established E2E channel.
//!
//! The crypto + frame codec live in the host-tested core
//! ([`baybo_mobile_core::ContentHandshake`] / [`ContentSession`]); this file is
//! just the WebSocket transport + the Tauri glue: a long-running task owns the
//! socket and the `ContentSession`, decrypts inbound frames onto a Tauri
//! [`Channel`] for the UI, and seals outbound user messages it receives over an
//! mpsc from [`content_send`](crate::content_send).
//!
//! One session at a time (phase 1): opening a new one aborts the previous task.
//! Relay content is phase 2 — a `Relay` endpoint in the plan is skipped for now.

use baybo_mobile_core::{
    ConnectError, ContentHandshake, ContentSession, Endpoint, Frame, connect_first, endpoints,
    subscribe_frame, user_text_frame,
};
use device_proto::noise::StaticKeypair;
use futures_util::{SinkExt, StreamExt};
use tauri::ipc::Channel;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::pairing::{PairedRecord, load_paired_record};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The single live content session, if any. Replaced (and the prior task
/// aborted) every time the UI opens a new one.
#[derive(Default)]
pub struct ContentSessions(Mutex<Option<ContentHandle>>);

struct ContentHandle {
    /// User messages the UI submits, handed to the pump task to seal + send.
    outbound_tx: mpsc::UnboundedSender<OutboundCmd>,
    task: tokio::task::JoinHandle<()>,
}

enum OutboundCmd {
    /// Send a user message; `msg_id` is the per-message idempotency key.
    Send { text: String, msg_id: String },
}

/// An established, handshaken session ready to pump.
struct Established {
    ws: Ws,
    session: ContentSession,
}

/// Open a content session for `session_id`, streaming decrypted gateway frames
/// to `on_frame`. Tries each direct candidate (relay is phase 2) until one
/// connects + completes the Noise handshake, then spawns the pump task.
pub async fn connect(
    sessions: &ContentSessions,
    session_id: String,
    on_frame: Channel<Frame>,
) -> Result<(), String> {
    let record = load_paired_record()?.ok_or("not paired; pair a gateway first")?;
    let local = StaticKeypair::from_parts(record.noise_public, record.noise_secret);
    let plan = endpoints(&record.direct_candidates, &record.relay_node_id);

    let record_ref = &record;
    let local_ref = &local;
    let established = connect_first(&plan, |ep| async move {
        match ep {
            Endpoint::Direct(base) => dial_direct(&base, record_ref, local_ref).await,
            Endpoint::Relay { node_id } => dial_relay(&node_id, record_ref, local_ref).await,
        }
    })
    .await;
    let established = match established {
        Ok(e) => e,
        Err(ConnectError::NoEndpoints) => {
            return Err("no reachable gateway endpoints; re-pair".into());
        }
        Err(ConnectError::AllFailed(e)) => return Err(format!("could not reach the gateway: {e}")),
    };

    let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
    let user_id = record.user_id.clone();
    let task = tokio::spawn(pump(
        established.ws,
        established.session,
        session_id,
        user_id,
        on_frame,
        outbound_rx,
    ));

    let mut guard = sessions.0.lock().await;
    if let Some(prev) = guard.take() {
        prev.task.abort();
    }
    *guard = Some(ContentHandle { outbound_tx, task });
    Ok(())
}

/// Queue a user message on the live session for the pump task to seal + send.
pub async fn send(sessions: &ContentSessions, text: String, msg_id: String) -> Result<(), String> {
    let guard = sessions.0.lock().await;
    let handle = guard.as_ref().ok_or("no active content session")?;
    handle
        .outbound_tx
        .send(OutboundCmd::Send { text, msg_id })
        .map_err(|_| "content session closed".to_string())
}

/// Tear down the live session (if any).
pub async fn disconnect(sessions: &ContentSessions) {
    if let Some(prev) = sessions.0.lock().await.take() {
        prev.task.abort();
    }
}

/// Dial one direct candidate and run the Noise IK initiator handshake.
async fn dial_direct(
    base: &str,
    record: &PairedRecord,
    local: &StaticKeypair,
) -> Result<Established, String> {
    let base = base.trim_end_matches('/');
    // The auth token rides the `?token=` query so the channel-auth middleware
    // resolves it to the device identity before the upgrade; the Noise IK
    // handshake then authenticates the static key end-to-end.
    let url = format!("{base}/v1/device/content?token={}", record.auth_token);
    let (ws, _) = connect_async(&url)
        .await
        .map_err(|e| format!("connect {base}: {e}"))?;
    handshake_over(ws, record, local).await
}

/// Dial the blind relay's content-join leg for the gateway's `relay_node_id`
/// (fallback when no direct candidate connected). No token: the relay leg is
/// unauthenticated and the gateway authenticates this device purely by matching
/// the Noise IK initiator's static against an approved device row.
async fn dial_relay(
    node_id: &str,
    record: &PairedRecord,
    local: &StaticKeypair,
) -> Result<Established, String> {
    if record.relay_url.is_empty() {
        return Err("no relay url for this pairing".into());
    }
    let base = record.relay_url.trim_end_matches('/');
    let url = remote_host_protocol::relay::content_join_url(base, node_id);
    let (ws, _) = connect_async(&url)
        .await
        .map_err(|e| format!("relay connect {base}: {e}"))?;
    handshake_over(ws, record, local).await
}

/// Run the Noise IK initiator handshake over an established WS (direct or relay)
/// and return the ready content session.
async fn handshake_over(
    mut ws: Ws,
    record: &PairedRecord,
    local: &StaticKeypair,
) -> Result<Established, String> {
    let (handshake, msg1) = ContentHandshake::start(local, &record.gateway_static_pubkey)
        .map_err(|e| format!("start handshake: {e}"))?;
    ws.send(Message::Binary(msg1))
        .await
        .map_err(|e| format!("send handshake: {e}"))?;
    let msg2 = recv_binary(&mut ws).await?;
    let session = handshake
        .finish(&msg2)
        .map_err(|e| format!("finish handshake: {e}"))?;
    Ok(Established { ws, session })
}

/// Read the next binary WS message (skipping ping/pong) — used for the Noise
/// handshake reply before the frame pump takes over.
async fn recv_binary(ws: &mut Ws) -> Result<Vec<u8>, String> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Binary(b))) => return Ok(b),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            Some(Ok(Message::Close(_))) | None => return Err("connection closed".into()),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(format!("ws: {e}")),
        }
    }
}

/// Own the socket + `ContentSession` for the session's lifetime: subscribe, then
/// fan inbound (decrypted) frames to `on_frame` and seal outbound user messages.
async fn pump(
    ws: Ws,
    mut session: ContentSession,
    session_id: String,
    user_id: String,
    on_frame: Channel<Frame>,
    mut outbound_rx: mpsc::UnboundedReceiver<OutboundCmd>,
) {
    let (mut sink, mut stream) = ws.split();

    // Self-pull: subscribe to the session so the gateway replays the thread and
    // streams live agent output.
    match session.seal(&subscribe_frame(&session_id, None)) {
        Ok(messages) => {
            for bytes in messages {
                if sink.send(Message::Binary(bytes)).await.is_err() {
                    return;
                }
            }
        }
        Err(_) => return,
    }

    'session: loop {
        tokio::select! {
            inbound = stream.next() => match inbound {
                Some(Ok(Message::Binary(bytes))) => {
                    // One Noise message may complete zero, one, or several frames.
                    let frames = match session.open(&bytes) {
                        Ok(f) => f,
                        // A decrypt failure means the Noise stream desynced —
                        // unrecoverable, so end the session.
                        Err(_) => break 'session,
                    };
                    for frame in frames {
                        // Answer the gateway's keepalive itself, don't forward it.
                        if matches!(frame, Frame::Ping) {
                            if let Ok(messages) = session.seal(&Frame::Pong) {
                                for b in messages {
                                    let _ = sink.send(Message::Binary(b)).await;
                                }
                            }
                            continue;
                        }
                        if on_frame.send(frame).is_err() {
                            // The webview dropped the channel (navigated away).
                            break 'session;
                        }
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | None => break 'session,
                Some(Ok(_)) => {}
                Some(Err(_)) => break 'session,
            },
            cmd = outbound_rx.recv() => match cmd {
                Some(OutboundCmd::Send { text, msg_id }) => {
                    let frame = user_text_frame(&session_id, &user_id, &text, &msg_id);
                    match session.seal(&frame) {
                        Ok(messages) => {
                            for bytes in messages {
                                if sink.send(Message::Binary(bytes)).await.is_err() {
                                    break 'session;
                                }
                            }
                        }
                        Err(_) => continue,
                    }
                }
                // Every sender dropped (handle replaced/torn down).
                None => break 'session,
            },
        }
    }
    let _ = sink.close().await;
}
