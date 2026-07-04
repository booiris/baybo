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
//! Everything else — coalesced reconnect, the connect timeout, the 45s inbound-
//! liveness watchdog, local Ping→Pong, per-session sink fan-out, and sealing
//! outbound user messages — is the one [`SessionRegistry`] + [`pump`] below,
//! written once.
//!
//! Lifted from the Tauri shell's `transport.rs`; the webview `Channel<Frame>` is
//! now a [`FrameSink`] callback interface, and the app-wide
//! `content-disconnected` event is the sink's `on_disconnected` — same contract:
//! it fires ONLY when the global leg ends on its own, because deliberate teardown
//! aborts the pump task before the call runs.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::api::FrameSink;
use crate::core::{Frame, MobileError, WireAttachment, fetch_history_frame, subscribe_frame};

/// The concrete client socket both legs dial.
pub(crate) type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Upper bound on a whole [`SessionRegistry::connect`] dial + handshake. Without
/// it a server that upgrades then never completes the handshake
/// would wedge `connect` with the `connecting` flag held, deadlocking every later
/// reconnect.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// If the socket yields nothing for this long the leg is treated as dead — e.g.
/// frozen across an iOS background round-trip, whose read side never resumes — and
/// the pump exits so the next foreground reconnect re-subscribes. The gateway
/// sends an application keepalive every 20s, so a silent window this long means
/// the socket is gone.
const INBOUND_LIVENESS_TIMEOUT: Duration = Duration::from_secs(45);

/// One error surface for both legs. Specific variants carry the few cases the
/// shared lifecycle distinguishes; [`TransportError::Other`] carries each leg's
/// own dial/handshake/REST prose verbatim (including the `invalid_token` code
/// that REST returns on a 401, folded into `BayboError::InvalidToken` at the FFI
/// boundary).
#[derive(Debug, thiserror::Error)]
pub(crate) enum TransportError {
    /// A setup / precondition failure — not paired, not signed in, no relay route,
    /// or a keychain read hiccup. Unlike a dead-leg failure it must NOT tear down a
    /// prior live session (see [`TransportError::should_reset_session`]).
    #[error("{0}")]
    Precondition(String),
    /// No subscribed session to send on.
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

/// Builds a leg's outbound user-message `Frame` (binds the target session id +
/// the leg's identity; both relay and direct use the device channel.
/// Owned + `Send` so the pump task holds it without borrowing the transport
/// across the spawn. Args are `(session_id, text, msg_id, attachments)`.
pub(crate) type UserFrameFn = Box<dyn Fn(&str, &str, &str, Vec<WireAttachment>) -> Frame + Send>;

/// A live, handshaken leg ready to pump: the socket, its frame codec, best-effort
/// opening frames (APNs for relay), and the outbound frame builder. The initial
/// and later `Subscribe` frames are ordinary outbound commands so one socket can
/// carry many sessions.
pub(crate) struct Connection {
    pub ws: WsStream,
    pub codec: Box<dyn FrameCodec>,
    /// Best-effort opening frames (e.g. the relay's APNs token refresh): an encode
    /// failure is skipped, not fatal — a transient failure to refresh push must not
    /// kill an otherwise-healthy session. A send failure is still fatal (socket gone).
    pub opening_best_effort: Vec<Frame>,
    pub user_frame: UserFrameFn,
}

/// Seam 2: a chat leg. `establish` is the only divergent step (dial + handshake +
/// auth); the rest of the lifecycle is the shared [`SessionRegistry`] below.
pub(crate) trait ChatTransport: Send + Sync {
    /// Dial, handshake, and authenticate the chat leg, returning the ready
    /// [`Connection`]. The explicit `+ Send` on the returned future (RPITIT) keeps
    /// it `Send` through the generic [`SessionRegistry::connect`] so the whole
    /// thing can run on the core runtime — no `async_trait` box needed.
    fn establish(
        &self,
    ) -> impl std::future::Future<Output = Result<Connection, TransportError>> + Send;
}

/// A request handed to the pump task to build + seal + send on the live leg.
enum OutboundCmd {
    /// Subscribe this global leg to a session and request forward catch-up.
    Subscribe {
        session_id: String,
        since_ordinal: Option<i64>,
    },
    /// A user message to send.
    Send {
        session_id: String,
        text: String,
        msg_id: String,
        attachments: Vec<WireAttachment>,
    },
    /// A backward transcript-history request. The reply (`Frame::HistoryPage`)
    /// arrives later through the normal inbound fan-out, not as a direct response.
    FetchHistory {
        session_id: String,
        before_ordinal: Option<i64>,
        limit: Option<u32>,
    },
}

/// The live pump for a binding: where to enqueue commands, and the task to abort
/// on teardown.
struct Handle {
    outbound_tx: mpsc::UnboundedSender<OutboundCmd>,
    task: tokio::task::JoinHandle<()>,
}

/// The pump slot plus a teardown epoch. The epoch fences a slow dial against a
/// teardown that raced it: [`SessionRegistry::disconnect`] bumps it, and a dial
/// that snapshotted an older epoch discards its connection instead of
/// installing an orphan pump.
#[derive(Default)]
struct HandleSlot {
    handle: Option<Handle>,
    epoch: u64,
}

/// The single live chat leg for one binding. The socket itself is global, while
/// `sinks` maps each subscribed session to the Swift owner that should receive
/// that session's frames.
pub(crate) struct SessionRegistry {
    slot: Mutex<HandleSlot>,
    connect_lock: Mutex<()>,
    sinks: Arc<Mutex<HashMap<String, Arc<dyn FrameSink>>>>,
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self {
            slot: Mutex::new(HandleSlot::default()),
            connect_lock: Mutex::new(()),
            sinks: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl SessionRegistry {
    /// Open the binding's global chat leg without subscribing any session. Used
    /// to warm the relay content leg at app launch so the first chat screen only
    /// needs to enqueue `Subscribe`.
    pub(crate) async fn preconnect<T: ChatTransport>(
        &self,
        transport: &T,
    ) -> Result<(), TransportError> {
        if self.has_live_pump().await {
            return Ok(());
        }

        let _connect = self.connect_lock.lock().await;
        if self.has_live_pump().await {
            return Ok(());
        }

        self.establish_pump(transport, "chat preconnect").await
    }

    /// Subscribe `session_id` on the binding's global chat leg, streaming that
    /// session's frames to `sink`. Concurrent first dials coalesce behind
    /// `connect_lock`; once a socket is live, this only sends another
    /// `Subscribe`.
    pub(crate) async fn connect<T: ChatTransport>(
        &self,
        transport: &T,
        session_id: &str,
        since_ordinal: Option<i64>,
        sink: Arc<dyn FrameSink>,
    ) -> Result<(), TransportError> {
        self.sinks.lock().await.insert(session_id.to_string(), sink);

        let subscribe = OutboundCmd::Subscribe {
            session_id: session_id.to_string(),
            since_ordinal,
        };
        if self.try_enqueue(subscribe).await? {
            return Ok(());
        }

        let _connect = self.connect_lock.lock().await;
        let subscribe = OutboundCmd::Subscribe {
            session_id: session_id.to_string(),
            since_ordinal,
        };
        if self.try_enqueue(subscribe).await? {
            return Ok(());
        }

        self.establish_pump(transport, "chat connect").await?;

        let subscribe = OutboundCmd::Subscribe {
            session_id: session_id.to_string(),
            since_ordinal,
        };
        if self.try_enqueue(subscribe).await? {
            Ok(())
        } else {
            Err(TransportError::SessionClosed)
        }
    }

    async fn has_live_pump(&self) -> bool {
        let mut slot = self.slot.lock().await;
        let Some(handle) = slot.handle.as_ref() else {
            return false;
        };
        if !handle.outbound_tx.is_closed() && !handle.task.is_finished() {
            return true;
        }
        if let Some(prev) = slot.handle.take() {
            prev.task.abort();
        }
        false
    }

    async fn establish_pump<T: ChatTransport>(
        &self,
        transport: &T,
        operation: &str,
    ) -> Result<(), TransportError> {
        let dial_epoch = self.slot.lock().await.epoch;

        let conn = match tokio::time::timeout(CONNECT_TIMEOUT, transport.establish()).await {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => {
                let reset = e.should_reset_session();
                log::warn!("{operation} failed: {e} (reset_prior_session={reset})");
                if reset {
                    self.abort_if_epoch(dial_epoch).await;
                }
                return Err(e);
            }
            Err(_) => {
                log::warn!("{operation} timed out after {}s", CONNECT_TIMEOUT.as_secs());
                self.abort_if_epoch(dial_epoch).await;
                return Err(TransportError::Timeout);
            }
        };

        let mut slot = self.slot.lock().await;
        if slot.epoch != dial_epoch {
            drop(slot);
            let mut ws = conn.ws;
            let _ = ws.close(None).await;
            log::info!("{operation} discarded: superseded by a teardown mid-dial");
            return Err(TransportError::SessionClosed);
        }
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(pump(conn, self.sinks.clone(), outbound_rx));
        if let Some(prev) = slot.handle.take() {
            prev.task.abort();
        }
        slot.handle = Some(Handle { outbound_tx, task });
        Ok(())
    }

    async fn try_enqueue(&self, cmd: OutboundCmd) -> Result<bool, TransportError> {
        let mut slot = self.slot.lock().await;
        let Some(handle) = slot.handle.as_ref() else {
            return Ok(false);
        };
        match handle.outbound_tx.send(cmd) {
            Ok(()) => Ok(true),
            Err(_) => {
                if let Some(prev) = slot.handle.take() {
                    prev.task.abort();
                }
                Ok(false)
            }
        }
    }

    /// Abort the live pump only if no teardown/install happened since
    /// `dial_epoch` was snapshotted (the failed-dial reset path).
    async fn abort_if_epoch(&self, dial_epoch: u64) {
        let mut slot = self.slot.lock().await;
        if slot.epoch == dial_epoch
            && let Some(prev) = slot.handle.take()
        {
            prev.task.abort();
        }
    }

    /// Queue a user message on the live session for the pump to build + seal + send.
    pub(crate) async fn send(
        &self,
        session_id: String,
        text: String,
        msg_id: String,
        attachments: Vec<WireAttachment>,
    ) -> Result<(), TransportError> {
        if !self.sinks.lock().await.contains_key(&session_id) {
            return Err(TransportError::NotConnected);
        }
        if self
            .try_enqueue(OutboundCmd::Send {
                session_id,
                text,
                msg_id,
                attachments,
            })
            .await?
        {
            Ok(())
        } else {
            Err(TransportError::NotConnected)
        }
    }

    /// Queue a transcript-history request on the live session. The reply
    /// (`Frame::HistoryPage`) streams back through the session's sink — there is
    /// no synchronous return value (the page is consumed by the transcript's frame
    /// switch, mirroring how `Subscribe` catch-up replays arrive).
    pub(crate) async fn fetch_history(
        &self,
        session_id: String,
        before_ordinal: Option<i64>,
        limit: Option<u32>,
    ) -> Result<(), TransportError> {
        if !self.sinks.lock().await.contains_key(&session_id) {
            return Err(TransportError::NotConnected);
        }
        if self
            .try_enqueue(OutboundCmd::FetchHistory {
                session_id,
                before_ordinal,
                limit,
            })
            .await?
        {
            Ok(())
        } else {
            Err(TransportError::NotConnected)
        }
    }

    /// Tear down the live pump (if any) and fence out any dial in flight: the
    /// epoch bump makes a slow establish discard its connection instead of
    /// resurrecting a session the owner just tore down.
    pub(crate) async fn disconnect(&self) {
        let mut slot = self.slot.lock().await;
        slot.epoch += 1;
        if let Some(prev) = slot.handle.take() {
            prev.task.abort();
        }
        drop(slot);
        self.sinks.lock().await.clear();
    }
}

/// A chat leg, seen as "the thing that owns a [`SessionRegistry`]". Exposing the
/// registry through one trait lets the generic session fns below
/// ([`connect`]/[`send`]/[`fetch_history`]/[`disconnect`]) drive either leg, so
/// neither `RelaySessions` nor `DirectSessions` re-declares the same four
/// delegating wrappers. Each leg is already a [`ChatTransport`], so the bound
/// carries the `establish` seam the registry needs to open a session.
pub(crate) trait SessionLeg: ChatTransport {
    fn registry(&self) -> &SessionRegistry;
}

/// Open `leg`'s global chat connection without subscribing any session.
/// Stringifies the error for the FFI boundary.
pub(crate) async fn preconnect<L: SessionLeg>(leg: &L) -> Result<(), String> {
    leg.registry()
        .preconnect(leg)
        .await
        .map_err(|e| e.to_string())
}

/// Subscribe `leg`'s global chat connection to `session_id`, streaming frames to
/// `sink`.
/// Stringifies the error for the FFI boundary (the leg-specific establish prose,
/// including the `invalid_token` code, rides through verbatim).
pub(crate) async fn connect<L: SessionLeg>(
    leg: &L,
    session_id: String,
    since_ordinal: Option<i64>,
    sink: Arc<dyn FrameSink>,
) -> Result<(), String> {
    leg.registry()
        .connect(leg, &session_id, since_ordinal, sink)
        .await
        .map_err(|e| e.to_string())
}

/// Queue a user message on `leg`'s live session.
pub(crate) async fn send<L: SessionLeg>(
    leg: &L,
    session_id: String,
    text: String,
    msg_id: String,
    attachments: Vec<WireAttachment>,
) -> Result<(), String> {
    leg.registry()
        .send(session_id, text, msg_id, attachments)
        .await
        .map_err(|e| e.to_string())
}

/// Queue a backward transcript-history request on `leg`'s live session. The
/// `Frame::HistoryPage` reply streams back through the sink, so this returns once
/// the request is enqueued, not the page.
pub(crate) async fn fetch_history<L: SessionLeg>(
    leg: &L,
    session_id: String,
    before_ordinal: Option<i64>,
    limit: Option<u32>,
) -> Result<(), String> {
    leg.registry()
        .fetch_history(session_id, before_ordinal, limit)
        .await
        .map_err(|e| e.to_string())
}

/// Tear down `leg`'s live pump (if any).
pub(crate) async fn disconnect<L: SessionLeg>(leg: &L) {
    leg.registry().disconnect().await;
}

/// Own the socket for the binding's lifetime: send best-effort opening frames,
/// then fan inbound frames to per-session sinks and seal outbound user messages.
/// The codec hides whether bytes are Noise-sealed (relay) or raw msgpack
/// (direct), so this body is identical for both legs.
///
/// Returning means the session ended on its own, so the task calls
/// [`FrameSink::on_disconnected`] last. A deliberate teardown aborts this task
/// before the call runs, so it fires only on an unsolicited drop.
async fn pump(
    conn: Connection,
    sinks: Arc<Mutex<HashMap<String, Arc<dyn FrameSink>>>>,
    outbound_rx: mpsc::UnboundedReceiver<OutboundCmd>,
) {
    run_pump(conn, sinks.clone(), outbound_rx).await;
    let sinks: Vec<(String, Arc<dyn FrameSink>)> = sinks
        .lock()
        .await
        .iter()
        .map(|(session_id, sink)| (session_id.clone(), sink.clone()))
        .collect();
    for (session_id, sink) in sinks {
        sink.on_disconnected(session_id.clone());
    }
}

/// The pump body: send opening frames, then fan inbound frames to the right
/// session sink and seal outbound commands until the leg ends for any reason.
async fn run_pump(
    conn: Connection,
    sinks: Arc<Mutex<HashMap<String, Arc<dyn FrameSink>>>>,
    mut outbound_rx: mpsc::UnboundedReceiver<OutboundCmd>,
) {
    let Connection {
        ws,
        mut codec,
        opening_best_effort,
        user_frame,
    } = conn;
    let (mut sink_ws, mut stream) = ws.split();

    // Best-effort opening frames (the relay's APNs token refresh): skip on an encode
    // failure rather than kill an otherwise-healthy session. A send failure is still
    // the socket dying, so it stays fatal.
    for frame in &opening_best_effort {
        match codec.encode_outbound(frame) {
            Ok(messages) => {
                for bytes in messages {
                    if let Err(e) = sink_ws.send(Message::Binary(bytes)).await {
                        log::warn!(
                            "chat connection start failed: opening frame send failed (apns_refresh): {e}"
                        );
                        return;
                    }
                }
            }
            Err(e) => {
                log::warn!(
                    "opening frame skipped: encode failed (apns_refresh; push binding may go stale): {e}"
                );
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
                            dispatch_inbound_frame(&sinks, frame).await;
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
                    Some(OutboundCmd::Subscribe { session_id, since_ordinal }) => {
                        (
                            subscribe_frame(&session_id, since_ordinal),
                            format!("subscribe session={session_id}"),
                        )
                    }
                    Some(OutboundCmd::Send { session_id, text, msg_id, attachments }) => {
                        let frame = user_frame(&session_id, &text, &msg_id, attachments);
                        (frame, format!("send session={session_id} msg_id={msg_id}"))
                    }
                    Some(OutboundCmd::FetchHistory { session_id, before_ordinal, limit }) => {
                        (
                            fetch_history_frame(&session_id, before_ordinal, limit),
                            format!("fetch_history session={session_id}"),
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

async fn dispatch_inbound_frame(sinks: &Mutex<HashMap<String, Arc<dyn FrameSink>>>, frame: Frame) {
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
