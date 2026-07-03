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
//! frames to the [`FrameSink`], and sealing outbound user messages — is the one
//! [`SessionRegistry`] + [`pump`] below, written once.
//!
//! Lifted from the Tauri shell's `transport.rs`; the webview `Channel<Frame>` is
//! now a [`FrameSink`] callback interface, and the app-wide
//! `content-disconnected` event is the sink's `on_disconnected` — same contract:
//! it fires ONLY when the session ends on its own, because deliberate teardown
//! aborts the pump task before the call runs.

use std::sync::Arc;
use std::time::Duration;

use baybo_mobile_core::{Frame, MobileError, WireAttachment, fetch_history_frame};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::api::FrameSink;

/// The concrete client socket both legs dial.
pub(crate) type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Upper bound on a whole [`SessionRegistry::connect`] (dial + handshake + opening
/// frames). Without it a server that upgrades then never completes the handshake
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

/// Builds a leg's outbound `Frame::FetchHistory` (binds the live session id). The
/// gateway answers with a `Frame::HistoryPage` on the same leg, which the pump
/// streams to the sink like any inbound frame. Args are `(before_ordinal,
/// limit)`. Identity-agnostic, so both legs use the same builder.
pub(crate) type HistoryFrameFn = Box<dyn Fn(Option<i64>, Option<u32>) -> Frame + Send>;

/// A live, handshaken leg ready to pump: the socket, its frame codec, the frames
/// to send immediately (Subscribe [+ APNs for relay]), and the outbound frame
/// builders. Assembled by [`ChatTransport::establish`].
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
    pub history_frame: HistoryFrameFn,
}

/// Seam 2: a chat leg. `establish` is the only divergent step (dial + handshake +
/// auth); the rest of the lifecycle is the shared [`SessionRegistry`] below.
pub(crate) trait ChatTransport: Send + Sync {
    /// Dial, handshake, and authenticate `session_id`, returning the ready
    /// [`Connection`]. The explicit `+ Send` on the returned future (RPITIT) keeps
    /// it `Send` through the generic [`SessionRegistry::connect`] so the whole
    /// thing can run on the core runtime — no `async_trait` box needed.
    fn establish(
        &self,
        session_id: &str,
        since_ordinal: Option<i64>,
    ) -> impl std::future::Future<Output = Result<Connection, TransportError>> + Send;
}

/// A request handed to the pump task to build + seal + send on the live leg.
enum OutboundCmd {
    /// A user message to send.
    Send {
        text: String,
        msg_id: String,
        attachments: Vec<WireAttachment>,
    },
    /// A backward transcript-history request. The reply (`Frame::HistoryPage`)
    /// arrives later through the normal inbound fan-out, not as a direct response.
    FetchHistory {
        before_ordinal: Option<i64>,
        limit: Option<u32>,
    },
}

/// The live pump for a session: where to enqueue sends, and the task to abort on
/// teardown.
struct Handle {
    outbound_tx: mpsc::UnboundedSender<OutboundCmd>,
    task: tokio::task::JoinHandle<()>,
}

/// The pump slot plus a teardown epoch. The epoch fences a slow dial against a
/// teardown that raced it: [`SessionRegistry::disconnect`] bumps it, and a dial
/// that snapshotted an older epoch discards its connection instead of
/// installing an orphan pump (whose `user_frame` binds the pre-teardown session
/// id, so its sends would silently land in a dead conversation).
#[derive(Default)]
struct HandleSlot {
    handle: Option<Handle>,
    epoch: u64,
}

/// The single live session for one leg: the pump slot plus the in-flight dial's
/// session id. Each leg embeds one.
#[derive(Default)]
pub(crate) struct SessionRegistry {
    slot: Mutex<HandleSlot>,
    /// The session id of the dial currently in flight, if any. Concurrent
    /// connects for the SAME session coalesce (iOS fires several foreground
    /// signals per resume); a connect for a DIFFERENT session errors so the
    /// caller takes its failed-dial path and retries once the slot frees —
    /// a false coalesced `Ok` would report "connected" with no pump serving
    /// the new session's sink.
    connecting: parking_lot::Mutex<Option<String>>,
}

/// Clears [`SessionRegistry::connecting`] on every exit from `connect` (success,
/// error, or early `?`).
struct ConnectingGuard<'a>(&'a parking_lot::Mutex<Option<String>>);

impl Drop for ConnectingGuard<'_> {
    fn drop(&mut self) {
        *self.0.lock() = None;
    }
}

impl SessionRegistry {
    /// Open a session for `session_id`, streaming frames to `sink`. Coalesces
    /// concurrent same-session dials, bounds the whole establish with
    /// [`CONNECT_TIMEOUT`], and on any failure tears the prior handle down (so a
    /// stale pump can't keep accepting sends after a failed reconnect) — unless a
    /// teardown superseded this dial, which the epoch fence detects.
    pub(crate) async fn connect<T: ChatTransport>(
        &self,
        transport: &T,
        session_id: &str,
        since_ordinal: Option<i64>,
        sink: Arc<dyn FrameSink>,
    ) -> Result<(), TransportError> {
        {
            let mut connecting = self.connecting.lock();
            match connecting.as_deref() {
                Some(in_flight) if in_flight == session_id => {
                    log::debug!(
                        "chat connect coalesced with in-flight dial (session={session_id})"
                    );
                    return Ok(());
                }
                Some(_) => {
                    return Err(TransportError::Other(
                        "connection busy with another session".into(),
                    ));
                }
                None => *connecting = Some(session_id.to_string()),
            }
        }
        let _connecting = ConnectingGuard(&self.connecting);

        // Snapshot the teardown epoch BEFORE dialing: a disconnect that lands
        // while we're establishing bumps it, and we must not install then.
        let dial_epoch = self.slot.lock().await.epoch;

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
                // leaves a healthy session alone. Epoch-checked so a late-failing
                // stale dial can't kill a pump installed after a teardown.
                let reset = e.should_reset_session();
                log::warn!(
                    "chat connect failed: {e} (session={session_id} reset_prior_session={reset})"
                );
                if reset {
                    self.abort_if_epoch(dial_epoch).await;
                }
                return Err(e);
            }
            Err(_) => {
                log::warn!(
                    "chat connect timed out after {}s (session={session_id})",
                    CONNECT_TIMEOUT.as_secs()
                );
                self.abort_if_epoch(dial_epoch).await;
                return Err(TransportError::Timeout);
            }
        };

        let mut slot = self.slot.lock().await;
        if slot.epoch != dial_epoch {
            // A teardown raced this dial: close the fresh socket instead of
            // installing an orphan pump. The caller's own supersede guard
            // swallows the error.
            drop(slot);
            let mut ws = conn.ws;
            let _ = ws.close(None).await;
            log::info!(
                "chat connect discarded: superseded by a teardown mid-dial (session={session_id})"
            );
            return Err(TransportError::SessionClosed);
        }
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(pump(conn, session_id.to_string(), sink, outbound_rx));
        if let Some(prev) = slot.handle.take() {
            prev.task.abort();
        }
        slot.handle = Some(Handle { outbound_tx, task });
        Ok(())
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
        text: String,
        msg_id: String,
        attachments: Vec<WireAttachment>,
    ) -> Result<(), TransportError> {
        let guard = self.slot.lock().await;
        let handle = guard.handle.as_ref().ok_or(TransportError::NotConnected)?;
        handle
            .outbound_tx
            .send(OutboundCmd::Send {
                text,
                msg_id,
                attachments,
            })
            .map_err(|_| TransportError::SessionClosed)
    }

    /// Queue a transcript-history request on the live session. The reply
    /// (`Frame::HistoryPage`) streams back through the session's sink — there is
    /// no synchronous return value (the page is consumed by the transcript's frame
    /// switch, mirroring how `Subscribe` catch-up replays arrive).
    pub(crate) async fn fetch_history(
        &self,
        before_ordinal: Option<i64>,
        limit: Option<u32>,
    ) -> Result<(), TransportError> {
        let guard = self.slot.lock().await;
        let handle = guard.handle.as_ref().ok_or(TransportError::NotConnected)?;
        handle
            .outbound_tx
            .send(OutboundCmd::FetchHistory {
                before_ordinal,
                limit,
            })
            .map_err(|_| TransportError::SessionClosed)
    }

    /// Tear down the live pump (if any) and fence out any dial in flight: the
    /// epoch bump makes a slow establish discard its connection instead of
    /// resurrecting a session the owner just tore down. Any leg-specific durable
    /// state (e.g. the direct leg's stashed channel token) is owned by the
    /// transport, not here.
    pub(crate) async fn disconnect(&self) {
        let mut slot = self.slot.lock().await;
        slot.epoch += 1;
        if let Some(prev) = slot.handle.take() {
            prev.task.abort();
        }
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

/// The `FetchHistory` frame builder both legs use. It binds only the session id
/// (identity-agnostic — unlike the user-message builder, which encodes the leg's
/// device/web identity), so relay and direct share this verbatim.
pub(crate) fn session_history_frame(session_id: String) -> HistoryFrameFn {
    Box::new(move |before_ordinal, limit| fetch_history_frame(&session_id, before_ordinal, limit))
}

/// Open `leg`'s chat session for `session_id`, streaming frames to `sink`.
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
    text: String,
    msg_id: String,
    attachments: Vec<WireAttachment>,
) -> Result<(), String> {
    leg.registry()
        .send(text, msg_id, attachments)
        .await
        .map_err(|e| e.to_string())
}

/// Queue a backward transcript-history request on `leg`'s live session. The
/// `Frame::HistoryPage` reply streams back through the sink, so this returns once
/// the request is enqueued, not the page.
pub(crate) async fn fetch_history<L: SessionLeg>(
    leg: &L,
    before_ordinal: Option<i64>,
    limit: Option<u32>,
) -> Result<(), String> {
    leg.registry()
        .fetch_history(before_ordinal, limit)
        .await
        .map_err(|e| e.to_string())
}

/// Tear down `leg`'s live pump (if any). Any leg-specific durable state (e.g. the
/// direct leg's stashed channel token) is owned by the leg, not the registry, so it
/// survives.
pub(crate) async fn disconnect<L: SessionLeg>(leg: &L) {
    leg.registry().disconnect().await;
}

/// Own the socket for the session's lifetime: send the opening frames, then fan
/// inbound frames to the sink and seal outbound user messages. The codec hides
/// whether bytes are Noise-sealed (relay) or raw msgpack (direct), so this body is
/// identical for both legs.
///
/// Returning means the session ended on its own, so the task calls
/// [`FrameSink::on_disconnected`] last. A deliberate teardown aborts this task
/// before the call runs, so it fires only on an unsolicited drop.
async fn pump(
    conn: Connection,
    session_id: String,
    sink: Arc<dyn FrameSink>,
    outbound_rx: mpsc::UnboundedReceiver<OutboundCmd>,
) {
    run_pump(conn, &session_id, sink.as_ref(), outbound_rx).await;
    sink.on_disconnected(session_id);
}

/// The pump body: send the opening frames, then fan inbound frames to the sink
/// and seal outbound user messages until the session ends for any reason.
/// `session_id` is log context only — every exit path records its cause here,
/// because the disconnected callback above carries only the session id.
async fn run_pump(
    conn: Connection,
    session_id: &str,
    sink: &dyn FrameSink,
    mut outbound_rx: mpsc::UnboundedReceiver<OutboundCmd>,
) {
    let Connection {
        ws,
        mut codec,
        opening,
        opening_best_effort,
        user_frame,
        history_frame,
    } = conn;
    let (mut sink_ws, mut stream) = ws.split();

    // Required opening frames (Subscribe): an encode failure leaves the session
    // unusable, so bail.
    for frame in &opening {
        match codec.encode_outbound(frame) {
            Ok(messages) => {
                for bytes in messages {
                    if let Err(e) = sink_ws.send(Message::Binary(bytes)).await {
                        log::warn!(
                            "chat session start failed: opening frame send failed (subscribe, session={session_id}): {e}"
                        );
                        return;
                    }
                }
            }
            Err(e) => {
                log::warn!(
                    "chat session start failed: opening frame encode failed (subscribe, session={session_id}): {e}"
                );
                return;
            }
        }
    }

    // Best-effort opening frames (the relay's APNs token refresh): skip on an encode
    // failure rather than kill an otherwise-healthy session. A send failure is still
    // the socket dying, so it stays fatal.
    for frame in &opening_best_effort {
        match codec.encode_outbound(frame) {
            Ok(messages) => {
                for bytes in messages {
                    if let Err(e) = sink_ws.send(Message::Binary(bytes)).await {
                        log::warn!(
                            "chat session start failed: opening frame send failed (apns_refresh, session={session_id}): {e}"
                        );
                        return;
                    }
                }
            }
            Err(e) => {
                log::warn!(
                    "opening frame skipped: encode failed (apns_refresh, session={session_id}; push binding may go stale): {e}"
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
                                    "chat session ended: inbound frame decode failed (session={session_id}; a relay noise desync is unrecoverable): {e}"
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
                            match serde_json::to_string(&frame) {
                                Ok(json) => sink.on_frame(json),
                                Err(e) => log::warn!(
                                    "inbound frame dropped: JSON serialize failed (session={session_id}): {e}"
                                ),
                            }
                        }
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(frame))) => {
                        match frame {
                            Some(cf) => log::info!(
                                "chat session ended: socket closed by peer (code={} reason={:?}, session={session_id})",
                                u16::from(cf.code),
                                cf.reason
                            ),
                            None => log::info!(
                                "chat session ended: socket closed by peer (no close frame body, session={session_id})"
                            ),
                        }
                        break 'session;
                    }
                    None => {
                        log::info!(
                            "chat session ended: socket stream ended (session={session_id})"
                        );
                        break 'session;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        log::info!(
                            "chat session ended: socket read error (session={session_id}): {e}"
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
                    Some(OutboundCmd::Send { text, msg_id, attachments }) => {
                        let frame = user_frame(&text, &msg_id, attachments);
                        (frame, format!("send msg_id={msg_id}"))
                    }
                    Some(OutboundCmd::FetchHistory { before_ordinal, limit }) => {
                        (history_frame(before_ordinal, limit), "fetch_history".to_string())
                    }
                    None => {
                        log::debug!(
                            "chat session ended: outbound command channel closed (session={session_id})"
                        );
                        break 'session;
                    }
                };
                match codec.encode_outbound(&frame) {
                    Ok(messages) => {
                        for bytes in messages {
                            if let Err(e) = sink_ws.send(Message::Binary(bytes)).await {
                                log::warn!(
                                    "chat session ended: outbound send failed ({cmd_kind}, session={session_id}): {e}"
                                );
                                break 'session;
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "outbound frame seal failed; {cmd_kind} dropped, session stays up (session={session_id}): {e}"
                        );
                        continue;
                    }
                }
            }
            _ = &mut liveness => {
                log::info!(
                    "chat session ended: inbound liveness timeout after {}s (socket presumed dead, e.g. iOS background freeze; session={session_id})",
                    INBOUND_LIVENESS_TIMEOUT.as_secs()
                );
                break 'session;
            }
        }
    }
    let _ = sink_ws.close().await;
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
