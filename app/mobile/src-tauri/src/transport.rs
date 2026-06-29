//! The shared chat-transport core: one WebSocket frame pump + session lifecycle
//! that both legs run — the relay (Noise E2E) leg in [`crate::relay::chat`] and the
//! direct (raw-MessagePack) leg in [`crate::direct::chat`]. The two legs are
//! near-identical pumps; they diverge in only two seams, captured by the traits
//! here:
//!
//! * [`FrameCodec`] — how a `Frame` crosses the socket. Relay seals/chunks it in
//!   Noise (1..N binary messages; a decrypt desync is fatal); direct encodes it
//!   1:1 (an unknown future variant is skipped for forward-compat). The fork is
//!   encoded in [`FrameCodec::decode_inbound`]'s return so the pump can treat
//!   `Err` uniformly as "end the session".
//! * [`ChatTransport::establish`] — dial + handshake + auth, returning the live
//!   socket already wrapped as a ready [`Connection`]. This is the only divergent
//!   step; the retry/rotation/handshake details stay inside each impl.
//!
//! Everything else — coalesced reconnect, the connect timeout, tear-down-prior-
//! handle-on-failure, the 45s inbound-liveness watchdog, local Ping→Pong, fanning
//! frames to the webview `Channel`, and sealing outbound user messages — is the
//! one [`SessionRegistry`] + [`pump`] below, written once.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use baybo_mobile_core::{Frame, MobileError, WireAttachment};
use futures_util::{SinkExt, StreamExt};
use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// The concrete client socket both legs dial.
pub(crate) type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Tauri event the [`pump`] emits when a live session ends on its own — the socket
/// closed, the inbound-liveness watchdog lapsed, a Noise stream desynced, or the
/// webview dropped the frame channel — but NOT when the task is deliberately
/// aborted by a fresh [`SessionRegistry::connect`] / [`SessionRegistry::disconnect`]
/// (those cancel the task before the emit runs). The webview listens and reconnects
/// with backoff, so a chat that drops mid-session (e.g. a remote-host restart)
/// recovers without waiting for the next foreground. Payload is the session id.
/// Both legs share it — the unified pump emits for relay and direct alike.
pub(crate) const CONTENT_DISCONNECTED_EVENT: &str = "content-disconnected";

/// Upper bound on a whole [`SessionRegistry::connect`] (dial + handshake + opening
/// frames). Without it a server that upgrades then never completes the handshake
/// would wedge `connect` with the `connecting` flag held, deadlocking every later
/// reconnect. Bounds both legs now (the relay leg gained this for free).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// If the socket yields nothing for this long the leg is treated as dead — e.g.
/// frozen across an iOS background round-trip, whose read side never resumes — and
/// the pump exits so the next foreground reconnect re-subscribes. The gateway
/// sends an application keepalive every 20s, so a silent window this long means
/// the socket is gone.
const INBOUND_LIVENESS_TIMEOUT: Duration = Duration::from_secs(45);

/// One error surface for both legs. Specific variants carry the few cases the
/// shared lifecycle distinguishes; [`TransportError::Other`] carries each leg's
/// own dial/handshake/REST prose verbatim (including the `invalid_token` code that
/// REST returns on a 401, whose `Display` the webview matches).
#[derive(Debug, thiserror::Error)]
pub(crate) enum TransportError {
    /// A setup / precondition failure — not paired, not signed in, no relay route,
    /// or a keychain read hiccup. Unlike a dead-leg failure it must NOT tear down a
    /// prior live session (see [`TransportError::should_reset_session`]).
    #[error("{0}")]
    Precondition(String),
    /// No live session to send on / disconnect.
    #[error("no active session")]
    NotConnected,
    /// The live session's send half is gone (the pump exited); the next reconnect
    /// re-subscribes.
    #[error("session closed")]
    SessionClosed,
    /// The whole connect exceeded [`CONNECT_TIMEOUT`].
    #[error("connecting to Baybo timed out")]
    Timeout,
    /// A `Frame` seal/open failed (Noise crypto or msgpack).
    #[error(transparent)]
    Codec(#[from] MobileError),
    /// Any other dial / handshake / REST failure, carrying the leg's own message.
    #[error("{0}")]
    Other(String),
}

impl TransportError {
    /// Whether a failed [`SessionRegistry::connect`] should also tear down any
    /// prior live session. A [`Precondition`](Self::Precondition) failure leaves a
    /// healthy pump alone (the original legs only reset on a failed dial); a
    /// dead-leg failure (dial / handshake / timeout) tears it down so queued sends
    /// fail loudly and force a fresh reconnect instead of writing into a black hole.
    fn should_reset_session(&self) -> bool {
        !matches!(self, TransportError::Precondition(_))
    }
}

/// Seam 1: how a `Frame` crosses this leg's socket.
pub(crate) trait FrameCodec: Send {
    /// One outbound `Frame` → the binary WS message(s) to send. Relay seals + chunks
    /// (1..N); direct encodes (always 1).
    fn encode_outbound(&mut self, frame: &Frame) -> Result<Vec<Vec<u8>>, TransportError>;

    /// One inbound WS binary message → the `Frame`(s) it completes. Relay decrypts +
    /// reassembles (0..N; a desync is `Err`, which the pump treats as fatal); direct
    /// decodes (1, or `Ok(vec![])` to skip an unknown future variant). Mapping the
    /// fork into the return value lets the pump treat `Err` uniformly as "end the
    /// session" while direct never errors on a benign unknown variant.
    fn decode_inbound(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, TransportError>;
}

/// Builds a leg's outbound user-message `Frame` (binds the live session id + the
/// leg's identity: device/ios for relay, web-operator/http for direct). Owned +
/// `Send` so the pump task holds it without borrowing the transport across the
/// spawn. Args are `(text, msg_id, attachments)`.
pub(crate) type UserFrameFn = Box<dyn Fn(&str, &str, Vec<WireAttachment>) -> Frame + Send>;

/// A live, handshaken leg ready to pump: the socket, its frame codec, the frames
/// to send immediately (Subscribe [+ APNs for relay]), and the outbound
/// user-message builder. Assembled by [`ChatTransport::establish`].
pub(crate) struct Connection {
    pub ws: WsStream,
    pub codec: Box<dyn FrameCodec>,
    /// Required opening frames (e.g. Subscribe): an encode failure ends the session.
    pub opening: Vec<Frame>,
    /// Best-effort opening frames (e.g. the relay's APNs token refresh): an encode
    /// failure is skipped, not fatal — a transient failure to refresh push must not
    /// kill an otherwise-healthy session. A send failure is still fatal (socket gone).
    pub opening_best_effort: Vec<Frame>,
    pub user_frame: UserFrameFn,
}

/// Seam 2: a chat leg. `establish` is the only divergent step (dial + handshake +
/// auth); the rest of the lifecycle is the shared [`SessionRegistry`] below.
pub(crate) trait ChatTransport: Send + Sync {
    /// Dial, handshake, and authenticate `session_id`, returning the ready
    /// [`Connection`]. The explicit `+ Send` on the returned future (RPITIT) keeps
    /// it `Send` through the generic [`SessionRegistry::connect`] so the whole
    /// thing can run inside a Tauri command's future — no `async_trait` box needed.
    fn establish(
        &self,
        session_id: &str,
        since_ordinal: Option<i64>,
    ) -> impl std::future::Future<Output = Result<Connection, TransportError>> + Send;
}

/// A user message handed to the pump task to build + seal + send.
enum OutboundCmd {
    Send {
        text: String,
        msg_id: String,
        attachments: Vec<WireAttachment>,
    },
}

/// The live pump for a session: where to enqueue sends, and the task to abort on
/// teardown.
struct Handle {
    outbound_tx: mpsc::UnboundedSender<OutboundCmd>,
    task: tokio::task::JoinHandle<()>,
}

/// The single live session for one leg: the live pump handle plus a "currently
/// dialing" flag so concurrent connects coalesce (iOS fires several foreground
/// signals per resume). Each leg's Tauri-managed state embeds one.
#[derive(Default)]
pub(crate) struct SessionRegistry {
    handle: Mutex<Option<Handle>>,
    connecting: AtomicBool,
}

/// Clears [`SessionRegistry::connecting`] on every exit from `connect` (success,
/// error, or early `?`).
struct ConnectingGuard<'a>(&'a AtomicBool);

impl Drop for ConnectingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl SessionRegistry {
    /// Open a session for `session_id`, streaming frames to `on_frame`. Coalesces
    /// concurrent dials, bounds the whole establish with [`CONNECT_TIMEOUT`], and
    /// on any failure tears the prior handle down first so a stale pump can't keep
    /// accepting sends after a failed reconnect.
    pub(crate) async fn connect<T: ChatTransport>(
        &self,
        transport: &T,
        app: AppHandle,
        session_id: &str,
        since_ordinal: Option<i64>,
        on_frame: Channel<Frame>,
    ) -> Result<(), TransportError> {
        // Coalesce concurrent dials: each foreground reconnect drives the same UI
        // state, so a second in-flight dial is pure waste. `swap` claims the slot.
        if self.connecting.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let _connecting = ConnectingGuard(&self.connecting);

        let conn = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            transport.establish(session_id, since_ordinal),
        )
        .await
        {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => {
                // Tear the prior pump down only for a dead-leg failure, so a stale
                // pump can't keep accepting sends after the leg went down. A
                // precondition failure (e.g. a transient keychain read on reconnect)
                // leaves a healthy session alone.
                if e.should_reset_session() {
                    self.disconnect().await;
                }
                return Err(e);
            }
            Err(_) => {
                self.disconnect().await;
                return Err(TransportError::Timeout);
            }
        };

        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(pump(
            conn,
            app,
            session_id.to_string(),
            on_frame,
            outbound_rx,
        ));

        let mut guard = self.handle.lock().await;
        if let Some(prev) = guard.take() {
            prev.task.abort();
        }
        *guard = Some(Handle { outbound_tx, task });
        Ok(())
    }

    /// Queue a user message on the live session for the pump to build + seal + send.
    pub(crate) async fn send(
        &self,
        text: String,
        msg_id: String,
        attachments: Vec<WireAttachment>,
    ) -> Result<(), TransportError> {
        let guard = self.handle.lock().await;
        let handle = guard.as_ref().ok_or(TransportError::NotConnected)?;
        handle
            .outbound_tx
            .send(OutboundCmd::Send {
                text,
                msg_id,
                attachments,
            })
            .map_err(|_| TransportError::SessionClosed)
    }

    /// Tear down the live pump (if any). Any leg-specific durable state (e.g. the
    /// direct leg's stashed channel token) is owned by the transport, not here.
    pub(crate) async fn disconnect(&self) {
        if let Some(prev) = self.handle.lock().await.take() {
            prev.task.abort();
        }
    }
}

/// Own the socket for the session's lifetime: send the opening frames, then fan
/// inbound frames to the webview and seal outbound user messages. The codec hides
/// whether bytes are Noise-sealed (relay) or raw msgpack (direct), so this body is
/// identical for both legs.
///
/// Returning means the session ended on its own, so the task emits
/// [`CONTENT_DISCONNECTED_EVENT`] last. A deliberate teardown aborts this task
/// before the emit runs, so the event fires only on an unsolicited drop.
async fn pump(
    conn: Connection,
    app: AppHandle,
    session_id: String,
    on_frame: Channel<Frame>,
    outbound_rx: mpsc::UnboundedReceiver<OutboundCmd>,
) {
    run_pump(conn, on_frame, outbound_rx).await;
    let _ = app.emit(CONTENT_DISCONNECTED_EVENT, &session_id);
}

/// The pump body: send the opening frames, then fan inbound frames to the webview
/// and seal outbound user messages until the session ends for any reason.
async fn run_pump(
    conn: Connection,
    on_frame: Channel<Frame>,
    mut outbound_rx: mpsc::UnboundedReceiver<OutboundCmd>,
) {
    let Connection {
        ws,
        mut codec,
        opening,
        opening_best_effort,
        user_frame,
    } = conn;
    let (mut sink, mut stream) = ws.split();

    // Required opening frames (Subscribe): an encode failure leaves the session
    // unusable, so bail.
    for frame in &opening {
        match codec.encode_outbound(frame) {
            Ok(messages) => {
                for bytes in messages {
                    if sink.send(Message::Binary(bytes)).await.is_err() {
                        return;
                    }
                }
            }
            Err(_) => return,
        }
    }

    // Best-effort opening frames (the relay's APNs token refresh): skip on an encode
    // failure rather than kill an otherwise-healthy session. A send failure is still
    // the socket dying, so it stays fatal.
    for frame in &opening_best_effort {
        if let Ok(messages) = codec.encode_outbound(frame) {
            for bytes in messages {
                if sink.send(Message::Binary(bytes)).await.is_err() {
                    return;
                }
            }
        }
    }

    let liveness = tokio::time::sleep(INBOUND_LIVENESS_TIMEOUT);
    tokio::pin!(liveness);

    'session: loop {
        tokio::select! {
            inbound = stream.next() => {
                liveness
                    .as_mut()
                    .reset(tokio::time::Instant::now() + INBOUND_LIVENESS_TIMEOUT);
                match inbound {
                    Some(Ok(Message::Binary(bytes))) => {
                        // Relay: a decrypt desync is fatal (Err → break). Direct: an
                        // unknown future variant decodes to Ok(vec![]) and is skipped.
                        let frames = match codec.decode_inbound(&bytes) {
                            Ok(frames) => frames,
                            Err(_) => break 'session,
                        };
                        for frame in frames {
                            // Answer the gateway's keepalive locally; never forward it.
                            if matches!(frame, Frame::Ping) {
                                if let Ok(messages) = codec.encode_outbound(&Frame::Pong) {
                                    for bytes in messages {
                                        let _ = sink.send(Message::Binary(bytes)).await;
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
                }
            }
            cmd = outbound_rx.recv() => match cmd {
                Some(OutboundCmd::Send { text, msg_id, attachments }) => {
                    let frame = user_frame(&text, &msg_id, attachments);
                    match codec.encode_outbound(&frame) {
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
                None => break 'session,
            },
            _ = &mut liveness => break 'session,
        }
    }
    let _ = sink.close().await;
}

/// Read the next binary WS message (skipping ping/pong) — used by both legs'
/// `establish` for the handshake reply before the pump takes over.
pub(crate) async fn recv_binary(ws: &mut WsStream) -> Result<Vec<u8>, TransportError> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Binary(b))) => return Ok(b),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            Some(Ok(Message::Close(_))) | None => {
                return Err(TransportError::Other("connection closed".into()));
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(TransportError::Other(format!("ws: {e}"))),
        }
    }
}
