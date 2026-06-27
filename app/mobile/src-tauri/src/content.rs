//! The content-session command: dial the gateway over the relay's content-join
//! leg, run the Noise IK handshake, and pump `Frame`s between the webview and the
//! gateway over the established E2E channel.
//!
//! The crypto + frame codec live in the host-tested core
//! ([`baybo_mobile_core::ContentHandshake`] / [`ContentSession`]); this file is
//! just the WebSocket transport + the Tauri glue: a long-running task owns the
//! socket and the `ContentSession`, decrypts inbound frames onto a Tauri
//! [`Channel`] for the UI, and seals outbound user messages it receives over an
//! mpsc from [`content_send`](crate::content_send).
//!
//! One session at a time: opening a new one aborts the previous task. Content is
//! relay-only — the app reaches the (possibly NAT'd) gateway through C's blind
//! content-join leg.

use std::time::Duration;

use baybo_mobile_core::{
    ContentHandshake, ContentSession, Frame, subscribe_frame, user_text_frame,
};
use device_proto::noise::StaticKeypair;
use futures_util::{SinkExt, StreamExt};
use tauri::ipc::Channel;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::{Error as WsError, Message, http::StatusCode};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::pairing::{PairedRecord, load_paired_record};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Retry budget for the content dial while the gateway's relay control link is
/// briefly absent. The gateway re-dials the relay on a fixed backoff after any
/// drop (5s `RECONNECT_BACKOFF` in the gateway's `relay_content`), so a phone
/// that opens chat inside that window would otherwise get a hard error; this
/// budget outlasts it. Only a `503 gateway not connected` is retried — a
/// permanent refusal (e.g. `401` for an unadmitted key) surfaces at once.
const CONTENT_DIAL_RETRIES: usize = 14;
const CONTENT_DIAL_RETRY_DELAY: Duration = Duration::from_millis(500);

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
/// to `on_frame`. Dials the gateway over the relay's content-join leg, completes
/// the Noise handshake, then spawns the pump task.
pub async fn connect(
    sessions: &ContentSessions,
    session_id: String,
    since_ordinal: Option<i64>,
    on_frame: Channel<Frame>,
) -> Result<(), String> {
    let record = load_paired_record()?.ok_or("not paired; pair a gateway first")?;
    let local = StaticKeypair::from_parts(record.noise_public, record.noise_secret);
    if record.relay_node_id.is_empty() {
        return Err("paired gateway has no relay route; re-pair".into());
    }
    let established = dial_relay(&record.relay_node_id, &record, &local).await?;

    let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
    let device_id = record.device_id.clone();
    let task = tokio::spawn(pump(
        established.ws,
        established.session,
        session_id,
        device_id,
        since_ordinal,
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

/// Dial the blind relay's content-join leg for the gateway's `relay_node_id`.
/// The relay admits this leg by the instance key (symmetric with the gateway's
/// host leg); end-to-end, the gateway authenticates this device by matching the
/// Noise IK initiator's static against an approved device row.
///
/// Retries a `503 gateway not connected` (the gateway is offline or mid-reconnect
/// to the relay) for a bounded window; every other failure surfaces at once.
async fn dial_relay(
    node_id: &str,
    record: &PairedRecord,
    local: &StaticKeypair,
) -> Result<Established, String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    if record.relay_url.is_empty() {
        return Err("no relay url for this pairing".into());
    }
    let base = record.relay_url.trim_end_matches('/');
    let url = remote_host_protocol::relay::content_join_url(base, node_id);

    let mut attempt = 0usize;
    let ws = loop {
        // Rebuilt per attempt (`into_client_request` yields an owned request).
        // Present the admission key the QR carried at pairing — the relay admits
        // the phone leg too now.
        let mut req = url
            .as_str()
            .into_client_request()
            .map_err(|e| format!("bad relay url {base}: {e}"))?;
        if !record.instance_key.is_empty() {
            let value = record
                .instance_key
                .parse()
                .map_err(|e| format!("bad instance key header: {e}"))?;
            req.headers_mut()
                .insert(remote_host_protocol::relay::INSTANCE_KEY_HEADER, value);
        }
        match connect_async(req).await {
            Ok((ws, _)) => break ws,
            // The relay's `content_join` returns 503 while no gateway holds a live
            // control connection for this node; it re-dials the relay on a fixed
            // backoff, so retry briefly rather than failing the open.
            Err(WsError::Http(resp))
                if resp.status() == StatusCode::SERVICE_UNAVAILABLE
                    && attempt < CONTENT_DIAL_RETRIES =>
            {
                attempt += 1;
                tokio::time::sleep(CONTENT_DIAL_RETRY_DELAY).await;
            }
            Err(WsError::Http(resp)) if resp.status() == StatusCode::SERVICE_UNAVAILABLE => {
                return Err(format!(
                    "gateway offline: {base} has no relay control connection; ensure the paired gateway is running with relay enabled"
                ));
            }
            Err(e) => return Err(format!("relay connect {base}: {e}")),
        }
    };
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
    device_id: String,
    since_ordinal: Option<i64>,
    on_frame: Channel<Frame>,
    mut outbound_rx: mpsc::UnboundedReceiver<OutboundCmd>,
) {
    let (mut sink, mut stream) = ws.split();

    // Self-pull: subscribe to the session so the gateway streams live agent output
    // and replays any thread rows above `since_ordinal` (the catch-up gap on a
    // reconnect after the app was backgrounded; `None` = no catch-up).
    match session.seal(&subscribe_frame(&session_id, since_ordinal)) {
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
                    let frame = user_text_frame(&session_id, &device_id, &text, &msg_id);
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
