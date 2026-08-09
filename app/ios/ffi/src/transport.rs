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
//! Frames reach the app through the [`FrameSink`] callback interface, and a lost
//! global leg surfaces as that sink's `on_disconnected` — which fires ONLY when
//! the leg ends on its own, because deliberate teardown aborts the pump task
//! before the call runs.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::api::{DeckSink, FrameSink, SessionListSink};
use crate::core::{
    Frame, MobileError, WireApprovalDecision, WireAttachment, resolve_approval_frame,
    subscribe_frame,
};

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

/// How long either leg's `establish` waits for the reply to its handshake — the
/// relay's Noise message 2, the direct leg's `RegisterAck`. Deliberately far
/// under [`CONNECT_TIMEOUT`]: a dial that dies here dies for a reason a redial
/// can fix, so it should cost one retry-ladder step, not the whole budget. Three
/// tries fit inside what a single unbounded wait used to burn.
const HANDSHAKE_REPLY_TIMEOUT: Duration = Duration::from_secs(6);

/// How long [`SessionRegistry::connect`] waits for the gateway's
/// `SubscribeState` — the frame it sends the moment a `Subscribe` registers —
/// before declaring the subscribe unproven. This is the whole point of the ack:
/// enqueueing a `Subscribe` proves only that a process-local channel accepted
/// it, so without waiting for a reply "connected" could describe a socket that
/// is a black hole, and nothing would notice until
/// [`INBOUND_LIVENESS_TIMEOUT`]. Generous enough for the bundle's several
/// storage reads on a cold gateway.
const SUBSCRIBE_ACK_TIMEOUT: Duration = Duration::from_secs(8);

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

/// A live, handshaken leg ready to pump: the socket, its frame codec, and the
/// outbound frame builder. The initial and later `Subscribe` frames are ordinary
/// outbound commands so one socket can carry many sessions.
pub(crate) struct Connection {
    pub ws: WsStream,
    pub codec: Box<dyn FrameCodec>,
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
    /// Subscribe this global leg to a session. The server replays no history —
    /// transcript recovery is the client's REST sync call.
    Subscribe { session_id: String },
    /// A user message to send.
    Send {
        session_id: String,
        text: String,
        msg_id: String,
        attachments: Vec<WireAttachment>,
    },
    /// The user's answer to a pending tool-approval prompt. Fire-and-forget:
    /// the gateway answers by resolving the gate and broadcasting
    /// `ApprovalResolved`, which arrives on the inbound frame path.
    ResolveApproval {
        call_id: String,
        decision: WireApprovalDecision,
    },
}

/// The live pump for a binding: where to enqueue commands, the task to abort on
/// teardown, and the leg's proof of life.
struct Handle {
    outbound_tx: mpsc::UnboundedSender<OutboundCmd>,
    task: tokio::task::JoinHandle<()>,
    /// When this leg last carried bytes the other way. Seeded at install (the
    /// handshake reply that produced the [`Connection`] is itself inbound
    /// traffic) and restamped by the pump on every inbound message — the same
    /// signal [`INBOUND_LIVENESS_TIMEOUT`] watches, read by
    /// [`SessionRegistry::await_subscribe_ack`] to tell a leg that has gone
    /// silent (retire it) from a leg that is delivering fine while one
    /// particular `Subscribe` went unanswered (leave it alone).
    last_inbound: Arc<parking_lot::Mutex<Instant>>,
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

/// The connection-global session-activity sink, shared by both legs (only one is
/// active at a time). Set once at init; the pump reads it live so registration
/// order vs. leg establish doesn't matter. `parking_lot` (sync) because it's a
/// plain slot read on the frame path, never held across an await.
pub(crate) type SharedListSink = Arc<parking_lot::Mutex<Option<Arc<dyn SessionListSink>>>>;

/// The connection-global deck sink — the [`SharedListSink`] pattern. Deck
/// frames (`DeckCardData` / `DeckChanged`) are session-less broadcasts with no
/// subscription to route by, so they land here and never on a per-session
/// [`FrameSink`].
pub(crate) type SharedDeckSink = Arc<parking_lot::Mutex<Option<Arc<dyn DeckSink>>>>;

/// Connects parked in [`SessionRegistry::connect`] waiting for their session's
/// `SubscribeState`, keyed by session id. A `Vec` because two opens of the same
/// session can be in flight at once (a redial racing a foreground reconnect).
type SubscribeAcks = Arc<parking_lot::Mutex<HashMap<String, Vec<oneshot::Sender<()>>>>>;

/// Everything [`pump`] needs besides the socket: where inbound frames go, and
/// the two signals `connect` reads back off the inbound path — the leg's
/// proof-of-life stamp and the parked `SubscribeState` waiters.
struct PumpCtx {
    sinks: Arc<Mutex<HashMap<String, Arc<dyn FrameSink>>>>,
    list_sink: SharedListSink,
    deck_sink: SharedDeckSink,
    subscribe_acks: SubscribeAcks,
    last_inbound: Arc<parking_lot::Mutex<Instant>>,
}

/// The single live chat leg for one binding. The socket itself is global, while
/// `sinks` maps each subscribed session to the Swift owner that should receive
/// that session's frames.
pub(crate) struct SessionRegistry {
    slot: Mutex<HandleSlot>,
    connect_lock: Mutex<()>,
    sinks: Arc<Mutex<HashMap<String, Arc<dyn FrameSink>>>>,
    /// Connection-global `SessionActivity` pings (chat-list unread) land here.
    list_sink: SharedListSink,
    /// Connection-global deck pushes (`DeckCardData` / `DeckChanged`) land here.
    deck_sink: SharedDeckSink,
    /// Connects waiting for the gateway to acknowledge their `Subscribe`.
    subscribe_acks: SubscribeAcks,
    /// [`SUBSCRIBE_ACK_TIMEOUT`], injectable so the tests that must sit out an
    /// unanswered subscribe cost milliseconds instead of seconds.
    subscribe_ack_timeout: Duration,
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self {
            slot: Mutex::new(HandleSlot::default()),
            connect_lock: Mutex::new(()),
            sinks: Arc::new(Mutex::new(HashMap::new())),
            list_sink: Arc::new(parking_lot::Mutex::new(None)),
            deck_sink: Arc::new(parking_lot::Mutex::new(None)),
            subscribe_acks: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            subscribe_ack_timeout: SUBSCRIBE_ACK_TIMEOUT,
        }
    }
}

impl SessionRegistry {
    /// Shrink the subscribe-ack budget so a test can exercise the unanswered
    /// path without sleeping through the production one.
    #[cfg(test)]
    fn with_subscribe_ack_timeout(mut self, budget: Duration) -> Self {
        self.subscribe_ack_timeout = budget;
        self
    }

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
    /// `Subscribe`. Returns once the gateway has ACKNOWLEDGED the subscription
    /// (see [`Self::await_subscribe_ack`]) — enqueueing is not connecting.
    pub(crate) async fn connect<T: ChatTransport>(
        &self,
        transport: &T,
        session_id: &str,
        sink: Arc<dyn FrameSink>,
    ) -> Result<(), TransportError> {
        self.sinks.lock().await.insert(session_id.to_string(), sink);
        self.subscribe_and_prove(transport, session_id, "chat connect", || {
            vec![OutboundCmd::Subscribe {
                session_id: session_id.to_string(),
            }]
        })
        .await
    }

    /// The shared body of [`Self::connect`] and [`Self::connect_and_send`]: park
    /// the ack, get `cmds` onto a live pump (dialing one if there is none), then
    /// wait for the gateway to answer.
    ///
    /// `cmds` is a closure rather than a value because each of the three
    /// attempts below consumes the commands it enqueues.
    ///
    /// `connect_lock` is released before the ack wait, deliberately: it exists to
    /// coalesce DIALS, and a session parked on its own acknowledgement is not
    /// dialing. Holding it across the wait would put every other session's open
    /// behind this one's ack budget.
    async fn subscribe_and_prove<T: ChatTransport>(
        &self,
        transport: &T,
        session_id: &str,
        operation: &str,
        cmds: impl Fn() -> Vec<OutboundCmd>,
    ) -> Result<(), TransportError> {
        // Parked BEFORE anything reaches the wire: the gateway's bundle can
        // land while `try_enqueue_all` is still returning.
        let ack = self.park_subscribe_ack(session_id);

        let leg = match self.try_enqueue_all(cmds()).await? {
            Some(leg) => leg,
            None => {
                let _connect = self.connect_lock.lock().await;
                match self.try_enqueue_all(cmds()).await? {
                    Some(leg) => leg,
                    None => {
                        self.establish_pump(transport, operation).await?;
                        match self.try_enqueue_all(cmds()).await? {
                            Some(leg) => leg,
                            None => {
                                self.clear_subscribe_ack(session_id);
                                return Err(TransportError::SessionClosed);
                            }
                        }
                    }
                }
                // `_connect` drops here, ending the dial-coalescing window
                // before the ack wait below.
            }
        };

        self.await_subscribe_ack(ack, session_id, leg).await
    }

    /// Subscribe `session_id` and enqueue the first user message on the same
    /// outbound pump, preserving Subscribe-before-Message ordering for draft
    /// sessions that do not have a live subscription yet.
    pub(crate) async fn connect_and_send<T: ChatTransport>(
        &self,
        transport: &T,
        session_id: &str,
        sink: Arc<dyn FrameSink>,
        message: OutboundMessage,
    ) -> Result<(), TransportError> {
        self.sinks.lock().await.insert(session_id.to_string(), sink);
        self.subscribe_and_prove(transport, session_id, "chat connect+send", || {
            Vec::from(subscribe_and_send_cmds(
                session_id,
                &message.text,
                &message.msg_id,
                &message.attachments,
            ))
        })
        .await
    }

    /// Register this connect's interest in `session_id`'s `SubscribeState` so
    /// the inbound path can wake it. Prunes waiters whose connect has already
    /// gone away (a cancelled FFI call drops its receiver).
    fn park_subscribe_ack(&self, session_id: &str) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        let mut acks = self.subscribe_acks.lock();
        let waiters = acks.entry(session_id.to_string()).or_default();
        waiters.retain(|waiter| !waiter.is_closed());
        waiters.push(tx);
        rx
    }

    fn clear_subscribe_ack(&self, session_id: &str) {
        let mut acks = self.subscribe_acks.lock();
        if let Some(waiters) = acks.get_mut(session_id) {
            waiters.retain(|waiter| !waiter.is_closed());
            if waiters.is_empty() {
                acks.remove(session_id);
            }
        }
    }

    /// Block until the gateway answers this `Subscribe` with its
    /// `SubscribeState` bundle, which it sends the moment the subscription
    /// registers.
    ///
    /// The timeout is where the real work is, because it has to tell two very
    /// different failures apart. `leg` is the proof-of-life stamp of the pump
    /// the commands went onto:
    ///
    /// * The leg delivered NOTHING while we waited — it is a black hole (a
    ///   half-open socket, typically one dialed as the radio came up and then
    ///   silently orphaned). Retire it so every session on it redials. Retiring
    ///   DROPS the handle instead of aborting the task: closing the outbound
    ///   channel ends the pump's loop normally, which is what lets [`pump`] run
    ///   its `on_disconnected` fan-out. An `abort` here would strand every
    ///   OTHER subscribed session — their sinks would survive on a socket the
    ///   gateway no longer has their subscription on, with nothing to tell them.
    /// * The leg kept delivering other traffic — it is healthy and this one
    ///   subscribe was rejected or lost (e.g. a session scoped to another
    ///   channel, which the gateway answers with a `Notice` and no bundle).
    ///   Fail this connect only; tearing the leg down would put every other
    ///   session through a redial on someone else's behalf, forever.
    async fn await_subscribe_ack(
        &self,
        ack: oneshot::Receiver<()>,
        session_id: &str,
        leg: Arc<parking_lot::Mutex<Instant>>,
    ) -> Result<(), TransportError> {
        let parked_at = Instant::now();
        let budget = self.subscribe_ack_timeout;
        let acked = tokio::time::timeout(budget, ack).await;
        self.clear_subscribe_ack(session_id);
        if matches!(acked, Ok(Ok(()))) {
            return Ok(());
        }

        if *leg.lock() > parked_at {
            log::warn!(
                "subscribe session={session_id} unacknowledged after {budget:?}, but the leg is live; not touching it"
            );
            return Err(TransportError::NotConnected);
        }

        log::warn!(
            "subscribe session={session_id} unacknowledged after {budget:?} and the leg carried nothing; retiring it"
        );
        self.retire_pump(&leg).await;
        Err(TransportError::SessionClosed)
    }

    /// End the live pump if it is still `leg`'s, WITHOUT aborting it — see
    /// [`Self::await_subscribe_ack`] for why the difference matters. Dropping
    /// the handle drops the only outbound sender, so the pump's `recv` returns
    /// `None` and it winds down through its normal exit.
    ///
    /// No epoch bump: the epoch fences dials against a deliberate TEARDOWN, and
    /// a dial racing this one is producing the replacement leg we want.
    async fn retire_pump(&self, leg: &Arc<parking_lot::Mutex<Instant>>) {
        let mut slot = self.slot.lock().await;
        let ours = slot
            .handle
            .as_ref()
            .is_some_and(|handle| Arc::ptr_eq(&handle.last_inbound, leg));
        if ours {
            drop(slot.handle.take());
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
        // Seeded now, not at the pump's first poll: the handshake reply that
        // produced `conn` already crossed the wire, so the leg is proven as of
        // this instant.
        let last_inbound = Arc::new(parking_lot::Mutex::new(Instant::now()));
        let task = tokio::spawn(pump(
            conn,
            PumpCtx {
                sinks: self.sinks.clone(),
                list_sink: self.list_sink.clone(),
                deck_sink: self.deck_sink.clone(),
                subscribe_acks: self.subscribe_acks.clone(),
                last_inbound: last_inbound.clone(),
            },
            outbound_rx,
        ));
        if let Some(prev) = slot.handle.take() {
            prev.task.abort();
        }
        slot.handle = Some(Handle {
            outbound_tx,
            task,
            last_inbound,
        });
        Ok(())
    }

    async fn try_enqueue(&self, cmd: OutboundCmd) -> Result<bool, TransportError> {
        Ok(self.try_enqueue_all([cmd]).await?.is_some())
    }

    /// Enqueue `cmds` on the live pump, returning that leg's proof-of-life stamp
    /// (the caller's handle on WHICH leg took them) or `None` when there is no
    /// live pump to take them.
    async fn try_enqueue_all(
        &self,
        cmds: impl IntoIterator<Item = OutboundCmd>,
    ) -> Result<Option<Arc<parking_lot::Mutex<Instant>>>, TransportError> {
        let mut slot = self.slot.lock().await;
        let Some(handle) = slot.handle.as_ref() else {
            return Ok(None);
        };
        for cmd in cmds {
            if handle.outbound_tx.send(cmd).is_err() {
                if let Some(prev) = slot.handle.take() {
                    prev.task.abort();
                }
                return Ok(None);
            }
        }
        let leg = handle.last_inbound.clone();
        Ok(Some(leg))
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

    /// Queue the user's answer to a pending approval prompt. Unlike
    /// [`Self::send`] this takes no session: the prompt's `call_id` is what the
    /// gateway keys on, and the frame carries no session binding. It still
    /// needs the leg to be live — a decision made after the pump died is lost,
    /// and the gate times out server-side (fail-closed).
    pub(crate) async fn resolve_approval(
        &self,
        call_id: String,
        decision: WireApprovalDecision,
    ) -> Result<(), TransportError> {
        if self
            .try_enqueue(OutboundCmd::ResolveApproval { call_id, decision })
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

    /// Drop `session_id`'s sink from the global leg WITHOUT tearing the leg down
    /// (unlike [`disconnect`](Self::disconnect), a binding-wide teardown). Used by
    /// the client's LRU eviction of an idle, offscreen session: the shared pump
    /// and every other subscribed session stay live, while this session's inbound
    /// frames stop being delivered (dispatch drops them with a debug log). There
    /// is no wire-level unsubscribe, so the gateway keeps the subscription until
    /// the leg next cycles — a reconnect re-subscribes only sessions that still
    /// hold a sink, so an evicted one lapses on its own.
    pub(crate) async fn unsubscribe(&self, session_id: &str) {
        self.sinks.lock().await.remove(session_id);
    }

    /// Install (or clear) the connection-global session-activity sink. Idempotent;
    /// both legs point at the same foreign sink.
    pub(crate) fn set_list_sink(&self, sink: Option<Arc<dyn SessionListSink>>) {
        *self.list_sink.lock() = sink;
    }

    /// Install (or clear) the connection-global deck sink. Idempotent; both
    /// legs point at the same foreign sink.
    pub(crate) fn set_deck_sink(&self, sink: Option<Arc<dyn DeckSink>>) {
        *self.deck_sink.lock() = sink;
    }
}

/// The user message payload `connect_and_send` enqueues right after the
/// `Subscribe` — grouped so the connect+send signatures stay under the arg
/// limit and the trio can't drift apart.
pub(crate) struct OutboundMessage {
    pub text: String,
    pub msg_id: String,
    pub attachments: Vec<WireAttachment>,
}

fn subscribe_and_send_cmds(
    session_id: &str,
    text: &str,
    msg_id: &str,
    attachments: &[WireAttachment],
) -> [OutboundCmd; 2] {
    [
        OutboundCmd::Subscribe {
            session_id: session_id.to_string(),
        },
        OutboundCmd::Send {
            session_id: session_id.to_string(),
            text: text.to_string(),
            msg_id: msg_id.to_string(),
            attachments: attachments.to_vec(),
        },
    ]
}

/// A chat leg, seen as "the thing that owns a [`SessionRegistry`]". Exposing the
/// registry through one trait lets the generic session fns below
/// ([`connect`]/[`send`]/[`disconnect`]) drive either leg, so
/// neither `RelaySessions` nor `DirectSessions` re-declares the same
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
    sink: Arc<dyn FrameSink>,
) -> Result<(), String> {
    leg.registry()
        .connect(leg, &session_id, sink)
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

/// Echo the user's approval decision back on `leg`'s live pump.
pub(crate) async fn resolve_approval<L: SessionLeg>(
    leg: &L,
    call_id: String,
    decision: WireApprovalDecision,
) -> Result<(), String> {
    leg.registry()
        .resolve_approval(call_id, decision)
        .await
        .map_err(|e| e.to_string())
}

/// Subscribe `session_id` if needed and queue the user message behind that
/// subscription on the same live pump.
pub(crate) async fn connect_and_send<L: SessionLeg>(
    leg: &L,
    session_id: String,
    sink: Arc<dyn FrameSink>,
    message: OutboundMessage,
) -> Result<(), String> {
    leg.registry()
        .connect_and_send(leg, &session_id, sink, message)
        .await
        .map_err(|e| e.to_string())
}

/// Tear down `leg`'s live pump (if any).
pub(crate) async fn disconnect<L: SessionLeg>(leg: &L) {
    leg.registry().disconnect().await;
}

/// Drop one session's sink from `leg`'s global chat connection without tearing
/// the leg down — the client's LRU eviction of an idle, offscreen session.
pub(crate) async fn unsubscribe<L: SessionLeg>(leg: &L, session_id: &str) {
    leg.registry().unsubscribe(session_id).await;
}

/// Point `leg`'s registry at the connection-global session-activity sink.
pub(crate) fn set_list_sink<L: SessionLeg>(leg: &L, sink: Option<Arc<dyn SessionListSink>>) {
    leg.registry().set_list_sink(sink);
}

/// Point `leg`'s registry at the connection-global deck sink.
pub(crate) fn set_deck_sink<L: SessionLeg>(leg: &L, sink: Option<Arc<dyn DeckSink>>) {
    leg.registry().set_deck_sink(sink);
}

/// Own the socket for the binding's lifetime: fan inbound frames to per-session
/// sinks and seal outbound user messages.
/// The codec hides whether bytes are Noise-sealed (relay) or raw msgpack
/// (direct), so this body is identical for both legs.
///
/// Returning means the session ended on its own, so the task calls
/// [`FrameSink::on_disconnected`] last. A deliberate teardown aborts this task
/// before the call runs, so it fires only on an unsolicited drop.
async fn pump(conn: Connection, ctx: PumpCtx, outbound_rx: mpsc::UnboundedReceiver<OutboundCmd>) {
    let sinks = ctx.sinks.clone();
    run_pump(conn, ctx, outbound_rx).await;
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
                // The same signal, published so a parked `connect` can read it
                // (see `SessionRegistry::await_subscribe_ack`).
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

async fn dispatch_inbound_frame(ctx: &PumpCtx, frame: Frame) {
    let PumpCtx {
        sinks,
        list_sink,
        deck_sink,
        subscribe_acks,
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
        // TEE: this bundle is the gateway's acknowledgement of a `Subscribe`,
        // so it wakes whoever is parked in `connect` for that session — and
        // STILL routes per-session, because its payload (turn, work steps,
        // pending approvals, tasks) is what the transcript REPLACEs its view
        // with.
        Frame::SubscribeState { session_id, .. } => {
            let waiters = subscribe_acks.lock().remove(session_id.as_str());
            for waiter in waiters.unwrap_or_default() {
                let _ = waiter.send(());
            }
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

/// Read the next binary WS message (skipping ping/pong).
///
/// UNBOUNDED, and it must stay that way: this is not a handshake helper. It is
/// also `relay::tunnel::NoiseFrames::recv`, i.e. the read under every
/// REST-over-relay response frame and every blob chunk, whose budgets are owned
/// by their own callers (`POOLED_LEG_FIRST_BYTE_TIMEOUT`,
/// `TUNNEL_REQUEST_TIMEOUT`, `TUNNEL_HANDSHAKE_TIMEOUT`) and are far wider than
/// any handshake's. A blanket timeout here silently caps all of them — and a
/// 100 MiB upload's post-transfer wait, which is deliberately uncapped. Wrap the
/// CALL SITE that wants a bound; see [`recv_binary_handshake`].
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

/// [`recv_binary`] for the ONE thing that deserves a short leash: a leg's
/// handshake reply (the relay's Noise message 2, the direct leg's
/// `RegisterAck`).
///
/// The upgrade completing does not mean anyone is on the other end yet. The
/// relay parks a content-join leg the moment it accepts it, and only then does
/// the gateway dial in to claim it — so a gateway that fails to claim leaves the
/// phone blocked on a perfectly healthy socket with nothing ever coming back.
/// Left unbounded that costs the whole [`CONNECT_TIMEOUT`]; bounded, it costs
/// one step of the retry ladder.
pub(crate) async fn recv_binary_handshake(ws: &mut WsStream) -> Result<Vec<u8>, TransportError> {
    tokio::time::timeout(HANDSHAKE_REPLY_TIMEOUT, recv_binary(ws))
        .await
        .map_err(|_| {
            TransportError::Other(format!(
                "no handshake reply within {}s (the peer accepted the socket but never answered)",
                HANDSHAKE_REPLY_TIMEOUT.as_secs()
            ))
        })?
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;
    use tokio_tungstenite::{accept_async, client_async};

    use crate::core::{decode, encode, user_message_frame};

    /// Frames are built by decoding JSON rather than by struct literal: `Frame`
    /// carries a `chrono` timestamp the FFI crate doesn't otherwise depend on.
    fn frame(json: &str) -> Frame {
        serde_json::from_str(json).expect("decode test frame")
    }

    fn session_activity(session_id: &str, source: &str, at: &str) -> Frame {
        frame(&format!(
            r#"{{"kind":"session_activity","session_id":"{session_id}","source":"{source}","at":"{at}"}}"#
        ))
    }

    fn notice(session_id: &str, text: &str) -> Frame {
        frame(&format!(
            r#"{{"kind":"notice","session_id":"{session_id}","level":"info","text":"{text}"}}"#
        ))
    }

    fn approval_resolved(call_id: &str) -> Frame {
        frame(&format!(
            r#"{{"kind":"approval_resolved","call_id":"{call_id}","decision":"approve"}}"#
        ))
    }

    #[derive(Default)]
    struct RecordingSink {
        frames: parking_lot::Mutex<Vec<String>>,
        disconnects: parking_lot::Mutex<Vec<String>>,
    }

    impl RecordingSink {
        fn frames(&self) -> Vec<String> {
            self.frames.lock().clone()
        }

        fn kinds(&self) -> Vec<String> {
            self.frames()
                .iter()
                .map(|json| {
                    let value: serde_json::Value = serde_json::from_str(json).expect("parse frame");
                    value["kind"].as_str().unwrap_or_default().to_string()
                })
                .collect()
        }

        fn disconnects(&self) -> Vec<String> {
            self.disconnects.lock().clone()
        }
    }

    impl FrameSink for RecordingSink {
        fn on_frame(&self, frame_json: String) {
            self.frames.lock().push(frame_json);
        }

        fn on_disconnected(&self, session_id: String) {
            self.disconnects.lock().push(session_id);
        }
    }

    #[derive(Default)]
    struct RecordingListSink {
        activity: parking_lot::Mutex<Vec<(String, String, i64)>>,
        titles: parking_lot::Mutex<Vec<(String, String)>>,
        approvals: parking_lot::Mutex<Vec<(String, bool)>>,
        stale: parking_lot::Mutex<usize>,
    }

    impl SessionListSink for RecordingListSink {
        fn on_activity(&self, session_id: String, source: String, at_millis: i64) {
            self.activity.lock().push((session_id, source, at_millis));
        }

        fn on_title(&self, session_id: String, title: String) {
            self.titles.lock().push((session_id, title));
        }

        fn on_approval_pending(&self, session_id: String, pending: bool) {
            self.approvals.lock().push((session_id, pending));
        }

        fn on_list_stale(&self) {
            *self.stale.lock() += 1;
        }
    }

    #[derive(Default)]
    struct RecordingDeckSink {
        cards: parking_lot::Mutex<Vec<(String, u32, String)>>,
        changed: parking_lot::Mutex<usize>,
    }

    impl DeckSink for RecordingDeckSink {
        fn on_card_data(&self, card_id: String, seq: u32, payload: String) {
            self.cards.lock().push((card_id, seq, payload));
        }

        fn on_deck_changed(&self) {
            *self.changed.lock() += 1;
        }
    }

    struct Fixture {
        ctx: PumpCtx,
        list: Arc<RecordingListSink>,
        deck: Arc<RecordingDeckSink>,
    }

    impl Fixture {
        fn new(session_ids: &[&str]) -> (Self, Vec<Arc<RecordingSink>>) {
            let list = Arc::new(RecordingListSink::default());
            let deck = Arc::new(RecordingDeckSink::default());
            let mut map: HashMap<String, Arc<dyn FrameSink>> = HashMap::new();
            let mut sinks = Vec::new();
            for session_id in session_ids {
                let sink = Arc::new(RecordingSink::default());
                map.insert((*session_id).to_string(), sink.clone());
                sinks.push(sink);
            }
            (
                Self {
                    ctx: PumpCtx {
                        sinks: Arc::new(Mutex::new(map)),
                        list_sink: Arc::new(parking_lot::Mutex::new(Some(
                            list.clone() as Arc<dyn SessionListSink>
                        ))),
                        deck_sink: Arc::new(parking_lot::Mutex::new(Some(
                            deck.clone() as Arc<dyn DeckSink>
                        ))),
                        subscribe_acks: Arc::new(parking_lot::Mutex::new(HashMap::new())),
                        last_inbound: Arc::new(parking_lot::Mutex::new(Instant::now())),
                    },
                    list,
                    deck,
                },
                sinks,
            )
        }

        /// Drop the connection-global list sink: a leg can pump before Swift has
        /// installed one.
        fn without_list_sink(self) -> Self {
            *self.ctx.list_sink.lock() = None;
            self
        }

        /// Drop the connection-global deck sink: a leg can pump before Swift
        /// has installed one (or the deck tab was never opened).
        fn without_deck_sink(self) -> Self {
            *self.ctx.deck_sink.lock() = None;
            self
        }

        async fn dispatch(&self, frame: Frame) {
            dispatch_inbound_frame(&self.ctx, frame).await;
        }
    }

    /// The unread badge for a session the device has NEVER opened rides entirely on
    /// this: `SessionActivity` goes to the connection-global list sink and RETURNS.
    /// Delete the return and the frame falls through to per-session routing, finds
    /// no sink for an unopened session, and is dropped — the badge dies.
    #[tokio::test]
    async fn session_activity_goes_to_the_list_sink_and_never_to_a_session_sink() {
        let (fixture, sinks) = Fixture::new(&["s1"]);

        fixture
            .dispatch(session_activity("s1", "assistant", "2026-07-12T00:00:00Z"))
            .await;

        assert_eq!(
            fixture.list.activity.lock().as_slice(),
            [("s1".to_string(), "assistant".to_string(), 1_783_814_400_000)]
        );
        assert!(
            sinks[0].frames().is_empty(),
            "the activity ping must not reach the transcript sink"
        );
    }

    /// A ping for a session with no sink at all — the whole point of the special
    /// case.
    #[tokio::test]
    async fn session_activity_for_a_never_opened_session_still_reaches_the_list() {
        let (fixture, sinks) = Fixture::new(&["s1"]);

        fixture
            .dispatch(session_activity("unopened", "user", "2026-07-12T00:00:00Z"))
            .await;

        assert_eq!(fixture.list.activity.lock().len(), 1);
        assert_eq!(fixture.list.activity.lock()[0].0, "unopened");
        assert!(sinks[0].frames().is_empty());
    }

    /// The mark for "this conversation is blocked waiting on you" has to reach
    /// a device with NOTHING subscribed — that is the state the app is in
    /// while the user is looking at the chat list, and it is the only state in
    /// which the mark is useful. `Frame::ApprovalRequested` cannot do this
    /// (the gateway dispatches it to a session's subscribers only), which is
    /// why the bit rides a `SessionUpdated` broadcast instead.
    #[tokio::test]
    async fn an_approval_mark_for_a_never_opened_session_still_reaches_the_list() {
        let (fixture, _sinks) = Fixture::new(&[]);

        fixture
            .dispatch(Frame::SessionUpdated {
                session_id: "unopened".into(),
                patch: wire::SessionPatch {
                    approval_pending: Some(true),
                    ..Default::default()
                },
            })
            .await;

        assert_eq!(
            *fixture.list.approvals.lock(),
            vec![("unopened".to_string(), true)]
        );
    }

    /// The clear is load-bearing on its own: a gate nobody answers self-denies
    /// after five minutes and broadcasts NO resolution, so `false` here is the
    /// only thing that ever retires the mark on those turns.
    #[tokio::test]
    async fn an_approval_clear_rides_the_same_tee() {
        let (fixture, sinks) = Fixture::new(&["s1"]);

        fixture
            .dispatch(Frame::SessionUpdated {
                session_id: "s1".into(),
                patch: wire::SessionPatch {
                    approval_pending: Some(false),
                    ..Default::default()
                },
            })
            .await;

        assert_eq!(
            *fixture.list.approvals.lock(),
            vec![("s1".to_string(), false)]
        );
        // TEE, not a lane: the frame still reaches the session's own sink.
        assert_eq!(sinks[0].frames().len(), 1);
    }

    /// A patch with both fields fires both hops — the title path and the
    /// approval path are independent taps on one frame, not an either/or.
    #[tokio::test]
    async fn a_patch_carrying_a_title_and_an_approval_flag_fires_both_hops() {
        let (fixture, _sinks) = Fixture::new(&[]);

        fixture
            .dispatch(Frame::SessionUpdated {
                session_id: "s1".into(),
                patch: wire::SessionPatch {
                    title: Some("Reset password flow".into()),
                    approval_pending: Some(true),
                    ..Default::default()
                },
            })
            .await;

        assert_eq!(fixture.list.titles.lock().len(), 1);
        assert_eq!(fixture.list.approvals.lock().len(), 1);
    }

    /// A pin / archive / hide patch carries neither field and must stay silent
    /// on both — an absent `approval_pending` means "no change", never `false`.
    #[tokio::test]
    async fn a_patch_without_the_approval_field_changes_nothing() {
        let (fixture, _sinks) = Fixture::new(&[]);

        fixture
            .dispatch(Frame::SessionUpdated {
                session_id: "s1".into(),
                patch: wire::SessionPatch {
                    pinned: Some(true),
                    ..Default::default()
                },
            })
            .await;

        assert!(fixture.list.approvals.lock().is_empty());
        assert!(fixture.list.titles.lock().is_empty());
    }

    /// `Gap { session_id: None }` is the gateway's "I dropped a session-less
    /// broadcast" nudge — and the broadcast it most often drops is the
    /// `SessionActivity` announcing a session the device has never seen (a cron
    /// fire, say). It has no routing session id, so without the special case it
    /// falls into the fan-out branch and reaches **nobody** when no session is
    /// subscribed — which is exactly the state the app is in while the user is
    /// looking at the chat list. Fixture with NO sinks is the whole point.
    #[tokio::test]
    async fn a_session_less_gap_reaches_the_list_sink_with_nothing_subscribed() {
        let (fixture, _sinks) = Fixture::new(&[]);

        fixture.dispatch(Frame::Gap { session_id: None }).await;

        assert_eq!(
            *fixture.list.stale.lock(),
            1,
            "a session-less Gap must nudge the chat list to refetch, or a new \
             session never appears while the list is on screen",
        );
    }

    /// A `Gap` that DOES name a session is a transcript concern: it must keep
    /// its old route to that session's frame sink, and must NOT be mistaken for
    /// a list-refetch nudge.
    #[tokio::test]
    async fn a_session_scoped_gap_still_routes_to_its_transcript_sink() {
        let (fixture, sinks) = Fixture::new(&["s1"]);

        fixture
            .dispatch(Frame::Gap {
                session_id: Some("s1".into()),
            })
            .await;

        assert_eq!(sinks[0].kinds(), ["gap"]);
        assert_eq!(
            *fixture.list.stale.lock(),
            0,
            "a session-scoped gap is not a list-plane nudge",
        );
    }

    fn deck_card_data(card_id: &str, seq: u32, payload: &str) -> Frame {
        Frame::DeckCardData {
            card_id: card_id.to_string(),
            seq,
            payload: payload.to_string(),
        }
    }

    /// A deck push has no session to route by. Without the special case it
    /// would fan out to every per-session transcript sink (as an unknown
    /// frame) while a user parked on the Deck tab with nothing subscribed got
    /// nothing — the exact hole the connection-global sink exists to close.
    #[tokio::test]
    async fn deck_card_data_goes_to_the_deck_sink_and_never_to_a_session_sink() {
        let (fixture, sinks) = Fixture::new(&["s1"]);

        fixture
            .dispatch(deck_card_data("c1", 41, r#"{"used":0.4}"#))
            .await;

        assert_eq!(
            fixture.deck.cards.lock().as_slice(),
            [("c1".to_string(), 41, r#"{"used":0.4}"#.to_string())]
        );
        assert!(
            sinks[0].frames().is_empty(),
            "a deck push must never reach a transcript sink"
        );
        assert_eq!(*fixture.deck.changed.lock(), 0);
    }

    /// Same routing for the structural nudge, which carries nothing at all.
    #[tokio::test]
    async fn deck_changed_goes_to_the_deck_sink_and_never_to_a_session_sink() {
        let (fixture, sinks) = Fixture::new(&["s1"]);

        fixture.dispatch(Frame::DeckChanged).await;

        assert_eq!(*fixture.deck.changed.lock(), 1);
        assert!(fixture.deck.cards.lock().is_empty());
        assert!(
            sinks[0].frames().is_empty(),
            "the deck nudge must never reach a transcript sink"
        );
    }

    /// No deck sink installed (the Deck tab was never opened, or the leg
    /// pumped before Swift registered one): deck frames are DROPPED, never
    /// rerouted to the per-session sinks. The `GET /v1/deck` pull on the tab's
    /// first open repaints from the stored snapshot, so nothing is lost.
    #[tokio::test]
    async fn a_deck_frame_without_a_deck_sink_is_dropped_not_broadcast() {
        let (fixture, sinks) = Fixture::new(&["s1"]);
        let fixture = fixture.without_deck_sink();

        fixture.dispatch(deck_card_data("c1", 1, "{}")).await;
        fixture.dispatch(Frame::DeckChanged).await;

        assert!(sinks[0].frames().is_empty());
    }

    /// The deck special-cases must not swallow the fan-out path: any OTHER
    /// session-less frame (here the approval broadcast, standing in for every
    /// unknown future one) still reaches every session sink exactly as before,
    /// and never the deck sink.
    #[tokio::test]
    async fn a_session_less_non_deck_frame_still_broadcasts_past_the_deck_sink() {
        let (fixture, sinks) = Fixture::new(&["s1", "s2"]);

        fixture.dispatch(approval_resolved("call-1")).await;

        for sink in &sinks {
            assert_eq!(sink.kinds(), ["approval_resolved"]);
        }
        assert!(fixture.deck.cards.lock().is_empty());
        assert_eq!(*fixture.deck.changed.lock(), 0);
    }

    #[tokio::test]
    async fn session_activity_maps_both_activity_kinds_to_their_wire_spellings() {
        let (fixture, _sinks) = Fixture::new(&[]);

        fixture
            .dispatch(session_activity("s1", "user", "2026-07-12T00:00:00Z"))
            .await;
        fixture
            .dispatch(session_activity("s1", "assistant", "2026-07-12T00:00:00Z"))
            .await;

        let sources: Vec<String> = fixture
            .list
            .activity
            .lock()
            .iter()
            .map(|(_, source, _)| source.clone())
            .collect();
        assert_eq!(sources, ["user", "assistant"]);
    }

    /// No list sink installed yet (a leg that pumped before Swift registered one):
    /// the frame is still swallowed, never rerouted to a transcript.
    #[tokio::test]
    async fn session_activity_without_a_list_sink_is_dropped_not_broadcast() {
        let (fixture, sinks) = Fixture::new(&["s1"]);
        let fixture = fixture.without_list_sink();

        fixture
            .dispatch(session_activity("s1", "assistant", "2026-07-12T00:00:00Z"))
            .await;

        assert!(sinks[0].frames().is_empty());
    }

    /// `SessionUpdated{title}` fires `on_title` AND falls through to per-session
    /// routing — the code comment says NOT a return, and the transcript webview
    /// relies on still receiving the frame (it just ignores it).
    #[tokio::test]
    async fn a_title_patch_fires_on_title_and_still_falls_through_to_the_session_sink() {
        let (fixture, sinks) = Fixture::new(&["s1"]);

        fixture
            .dispatch(frame(
                r#"{"kind":"session_updated","session_id":"s1","patch":{"title":"A chat"}}"#,
            ))
            .await;

        assert_eq!(
            fixture.list.titles.lock().as_slice(),
            [("s1".to_string(), "A chat".to_string())]
        );
        assert_eq!(sinks[0].kinds(), ["session_updated"]);
    }

    /// Pin / archive / hide patches carry no title: no `on_title` hop, same
    /// fall-through.
    #[tokio::test]
    async fn a_titleless_patch_fires_no_title_hop_but_still_routes() {
        let (fixture, sinks) = Fixture::new(&["s1"]);

        fixture
            .dispatch(frame(
                r#"{"kind":"session_updated","session_id":"s1","patch":{"pinned":true}}"#,
            ))
            .await;

        assert!(fixture.list.titles.lock().is_empty());
        assert_eq!(sinks[0].kinds(), ["session_updated"]);
    }

    /// A session-less frame broadcasts to EVERY sink. The approval card is matched
    /// by prompt id precisely because of this: the gateway resolves a gate without
    /// naming a session, and whichever store holds that prompt must see it.
    #[tokio::test]
    async fn a_session_less_frame_broadcasts_to_every_sink() {
        let (fixture, sinks) = Fixture::new(&["s1", "s2", "s3"]);

        fixture.dispatch(approval_resolved("call-1")).await;

        for sink in &sinks {
            assert_eq!(sink.kinds(), ["approval_resolved"]);
            let json: serde_json::Value =
                serde_json::from_str(&sink.frames()[0]).expect("parse frame");
            assert_eq!(json["call_id"], "call-1");
        }
    }

    /// The ghost-row leak: a frame for a session this connection has no sink for
    /// (evicted by the LRU, or never opened) is DROPPED. Broadcasting it instead
    /// would paint another session's rows into the open transcript.
    #[tokio::test]
    async fn a_frame_for_an_unknown_session_is_dropped_not_broadcast() {
        let (fixture, sinks) = Fixture::new(&["s1", "s2"]);

        fixture.dispatch(notice("evicted", "hello")).await;

        for sink in &sinks {
            assert!(
                sink.frames().is_empty(),
                "a frame for an unsubscribed session must never fan out"
            );
        }
    }

    #[tokio::test]
    async fn a_session_frame_reaches_only_its_own_sink() {
        let (fixture, sinks) = Fixture::new(&["s1", "s2"]);

        fixture.dispatch(notice("s1", "for s1")).await;

        assert_eq!(sinks[0].kinds(), ["notice"]);
        assert!(sinks[1].frames().is_empty());
    }

    /// The sink receives the frame as JSON — the same shape the web transcript
    /// consumes, tagged on `kind`.
    #[tokio::test]
    async fn a_routed_frame_arrives_as_kind_tagged_json() {
        let (fixture, sinks) = Fixture::new(&["s1"]);

        fixture.dispatch(notice("s1", "hello")).await;

        let json: serde_json::Value =
            serde_json::from_str(&sinks[0].frames()[0]).expect("parse frame");
        assert_eq!(json["kind"], "notice");
        assert_eq!(json["session_id"], "s1");
        assert_eq!(json["text"], "hello");
    }

    /// A precondition failure (not paired / no credentials / a keychain hiccup on a
    /// foreground reconnect) must LEAVE a healthy pump alone; every dead-leg failure
    /// tears it down so queued sends fail loudly instead of writing into a black hole.
    #[test]
    fn only_a_precondition_failure_spares_a_live_session() {
        assert!(!TransportError::Precondition("not signed in".into()).should_reset_session());

        for dead_leg in [
            TransportError::NotConnected,
            TransportError::SessionClosed,
            TransportError::Timeout,
            TransportError::Other("ws connect: refused".into()),
            TransportError::Codec(MobileError::State("noise desync")),
        ] {
            assert!(
                dead_leg.should_reset_session(),
                "{dead_leg} must tear the prior session down"
            );
        }
    }

    /// The loopback leg: a real WebSocket on 127.0.0.1 speaking the direct leg's
    /// raw-MessagePack codec, so the registry + pump under test are the production
    /// ones and only the dial is local.
    struct LoopbackTransport {
        addr: std::net::SocketAddr,
        dials: Arc<AtomicUsize>,
    }

    struct LoopbackCodec;

    impl FrameCodec for LoopbackCodec {
        fn encode_outbound(&mut self, frame: &Frame) -> Result<Vec<Vec<u8>>, TransportError> {
            Ok(vec![encode(frame).map_err(MobileError::from)?])
        }

        fn decode_inbound(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, TransportError> {
            Ok(decode(bytes).ok().into_iter().collect())
        }
    }

    impl ChatTransport for LoopbackTransport {
        #[allow(clippy::manual_async_fn)]
        fn establish(
            &self,
        ) -> impl std::future::Future<Output = Result<Connection, TransportError>> + Send {
            async move {
                self.dials.fetch_add(1, Ordering::Relaxed);
                let tcp = TcpStream::connect(self.addr)
                    .await
                    .map_err(|e| TransportError::Other(format!("tcp: {e}")))?;
                let url = format!("ws://{}/v1/channel-ws", self.addr);
                let (ws, _) = client_async(url, MaybeTlsStream::Plain(tcp))
                    .await
                    .map_err(|e| TransportError::Other(format!("ws: {e}")))?;
                let user_frame: UserFrameFn = Box::new(|session_id, text, msg_id, attachments| {
                    user_message_frame(session_id, "device-1", text, msg_id, attachments)
                });
                Ok(Connection {
                    ws,
                    codec: Box::new(LoopbackCodec),
                    user_frame,
                })
            }
        }
    }

    /// What the test pushes at the client.
    enum ServerAction {
        Frame(Frame),
        Close(CloseFrame<'static>),
    }

    /// The gateway side of the loopback: serves connections (one at a time, so a
    /// redial is picked up), mirrors every frame the client sends into a channel
    /// the test drains, and — like the real gateway — answers each `Subscribe`
    /// with the `SubscribeState` bundle that acknowledges it.
    ///
    /// That ack is not decoration. `SessionRegistry::connect` now waits for it,
    /// so a loopback that stayed silent would no longer be a fake gateway; it
    /// would be a broken one, and every test here would sit out
    /// `SUBSCRIBE_ACK_TIMEOUT`. `Server::silent` is the deliberate opposite,
    /// used to test what happens when the ack never comes.
    struct Server {
        addr: std::net::SocketAddr,
        inbound: mpsc::UnboundedReceiver<Frame>,
        outbound: mpsc::UnboundedSender<ServerAction>,
    }

    impl Server {
        async fn start() -> Self {
            Self::spawn(true).await
        }

        /// A gateway that takes the `Subscribe` and never acknowledges it.
        async fn silent() -> Self {
            Self::spawn(false).await
        }

        async fn spawn(auto_ack: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");
            let (inbound_tx, inbound) = mpsc::unbounded_channel();
            let (outbound, mut outbound_rx) = mpsc::unbounded_channel();
            tokio::spawn(async move {
                loop {
                    let Ok((tcp, _)) = listener.accept().await else {
                        return;
                    };
                    let ws = accept_async(tcp).await.expect("ws handshake");
                    let (mut sink, mut stream) = ws.split();
                    loop {
                        tokio::select! {
                            message = stream.next() => {
                                let Some(Ok(Message::Binary(bytes))) = message else {
                                    break;
                                };
                                let frame = decode(&bytes).expect("decode client frame");
                                if auto_ack
                                    && let Frame::Subscribe { session_id } = &frame
                                {
                                    let ack = encode(&subscribe_state(session_id.as_str()))
                                        .expect("encode ack");
                                    if sink.send(Message::Binary(ack)).await.is_err() {
                                        break;
                                    }
                                }
                                if inbound_tx.send(frame).is_err() {
                                    return;
                                }
                            }
                            action = outbound_rx.recv() => {
                                match action {
                                    Some(ServerAction::Frame(frame)) => {
                                        let bytes = encode(&frame).expect("encode server frame");
                                        if sink.send(Message::Binary(bytes)).await.is_err() {
                                            break;
                                        }
                                    }
                                    Some(ServerAction::Close(close)) => {
                                        let _ = sink.send(Message::Close(Some(close))).await;
                                        break;
                                    }
                                    None => return,
                                }
                            }
                        }
                    }
                }
            });
            Self {
                addr,
                inbound,
                outbound,
            }
        }

        fn transport(&self) -> LoopbackTransport {
            LoopbackTransport {
                addr: self.addr,
                dials: Arc::new(AtomicUsize::new(0)),
            }
        }

        /// The next `Frame` the client sent, failing rather than hanging if the
        /// socket goes quiet.
        async fn next_frame(&mut self) -> Frame {
            tokio::time::timeout(Duration::from_secs(5), self.inbound.recv())
                .await
                .expect("client sends a frame in time")
                .expect("server task is alive")
        }

        /// Whether the client sent anything at all within `window`.
        async fn stayed_quiet(&mut self, window: Duration) -> bool {
            tokio::time::timeout(window, self.inbound.recv())
                .await
                .is_err()
        }

        fn send(&self, frame: Frame) {
            self.outbound
                .send(ServerAction::Frame(frame))
                .expect("server task is alive");
        }

        fn close(&self, close: CloseFrame<'static>) {
            self.outbound
                .send(ServerAction::Close(close))
                .expect("server task is alive");
        }
    }

    /// The acknowledgement bundle the gateway sends the moment a `Subscribe`
    /// registers (`crates/gateway/src/channel/route.rs`, `send_subscribe_state`).
    fn subscribe_state(session_id: &str) -> Frame {
        Frame::SubscribeState {
            session_id: session_id.into(),
            as_of_ordinal: None,
            turn: wire::TurnSnapshot {
                active: false,
                started_at: None,
            },
            work_steps: Vec::new(),
            pending_approvals: Vec::new(),
            tasks: Vec::new(),
        }
    }

    /// Subscribe MUST precede the first Message on a draft session's leg — the
    /// gateway drops a message for a session this connection isn't subscribed to,
    /// and this file has shipped that regression before.
    #[tokio::test]
    async fn connect_and_send_puts_subscribe_before_the_message() {
        let mut server = Server::start().await;
        let transport = server.transport();
        let registry = SessionRegistry::default();
        let sink = Arc::new(RecordingSink::default());

        registry
            .connect_and_send(
                &transport,
                "s1",
                sink,
                OutboundMessage {
                    text: "hello".to_string(),
                    msg_id: "m1".to_string(),
                    attachments: Vec::new(),
                },
            )
            .await
            .expect("connect and send");

        assert!(matches!(
            server.next_frame().await,
            Frame::Subscribe { session_id } if session_id.as_str() == "s1"
        ));
        match server.next_frame().await {
            Frame::Message(message) => {
                assert_eq!(message.session_id.as_str(), "s1");
                assert_eq!(message.content, "hello");
                assert_eq!(message.platform_msg_id, "m1");
            }
            other => panic!("expected the user message, got {other:?}"),
        }
    }

    /// `preconnect` warms the leg WITHOUT subscribing anything; the first Subscribe
    /// is the one `connect` sends. The warmed socket is then REUSED — opening a
    /// session must not redial (nor must switching between sessions, below).
    #[tokio::test]
    async fn preconnect_opens_the_leg_without_subscribing_a_session() {
        let mut server = Server::start().await;
        let transport = server.transport();
        let registry = SessionRegistry::default();

        registry.preconnect(&transport).await.expect("preconnect");

        assert!(
            server.stayed_quiet(Duration::from_millis(200)).await,
            "preconnect must not send a Subscribe"
        );

        registry
            .connect(&transport, "s1", Arc::new(RecordingSink::default()))
            .await
            .expect("connect");
        assert!(matches!(
            server.next_frame().await,
            Frame::Subscribe { session_id } if session_id.as_str() == "s1"
        ));
        assert_eq!(
            transport.dials.load(Ordering::Relaxed),
            1,
            "opening a session must reuse the warmed leg, not redial"
        );
    }

    /// One leg carries many sessions: subscribing a second session sends another
    /// Subscribe on the SAME socket and leaves the first subscription alone.
    #[tokio::test]
    async fn switching_sessions_reuses_the_one_leg() {
        let mut server = Server::start().await;
        let transport = server.transport();
        let registry = SessionRegistry::default();

        registry
            .connect(&transport, "s1", Arc::new(RecordingSink::default()))
            .await
            .expect("connect s1");
        registry
            .connect(&transport, "s2", Arc::new(RecordingSink::default()))
            .await
            .expect("connect s2");

        let subscribed: Vec<String> = vec![server.next_frame().await, server.next_frame().await]
            .into_iter()
            .map(|frame| match frame {
                Frame::Subscribe { session_id } => session_id.as_str().to_owned(),
                other => panic!("expected a Subscribe, got {other:?}"),
            })
            .collect();

        assert_eq!(subscribed, ["s1", "s2"]);
        assert_eq!(
            transport.dials.load(Ordering::Relaxed),
            1,
            "a second session must not open a second socket"
        );
    }

    /// The gateway's application keepalive is answered locally and NEVER forwarded:
    /// a `Ping` reaching the transcript would render as an unknown frame.
    #[tokio::test]
    async fn a_ping_is_answered_with_a_pong_and_never_forwarded_to_a_sink() {
        let mut server = Server::start().await;
        let transport = server.transport();
        let registry = SessionRegistry::default();
        let sink = Arc::new(RecordingSink::default());

        registry
            .connect(&transport, "s1", sink.clone())
            .await
            .expect("connect");
        assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));

        server.send(Frame::Ping);

        assert!(matches!(server.next_frame().await, Frame::Pong));
        // The subscribe ack DOES reach the sink (the transcript replaces its
        // state from it); the keepalive must not.
        assert_eq!(sink.kinds(), ["subscribe_state"]);
    }

    /// A deliberate teardown aborts the pump BEFORE it can report death, so logout
    /// doesn't kick the reconnect ladder against credentials that were just wiped.
    #[tokio::test]
    async fn a_deliberate_disconnect_never_fires_on_disconnected() {
        let mut server = Server::start().await;
        let transport = server.transport();
        let registry = SessionRegistry::default();
        let sink = Arc::new(RecordingSink::default());

        registry
            .connect(&transport, "s1", sink.clone())
            .await
            .expect("connect");
        assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));

        registry.disconnect().await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(
            sink.disconnects().is_empty(),
            "a deliberate teardown must not look like an unsolicited drop"
        );
    }

    /// The other half of that contract: an unsolicited drop (the peer closing) MUST
    /// fire `on_disconnected` for every subscribed session — that is what arms the
    /// reconnect ladder.
    #[tokio::test]
    async fn an_unsolicited_close_fires_on_disconnected_for_every_session() {
        let mut server = Server::start().await;
        let transport = server.transport();
        let registry = SessionRegistry::default();
        let first = Arc::new(RecordingSink::default());
        let second = Arc::new(RecordingSink::default());

        registry
            .connect(&transport, "s1", first.clone())
            .await
            .expect("connect s1");
        registry
            .connect(&transport, "s2", second.clone())
            .await
            .expect("connect s2");
        assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));
        assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));

        server.close(CloseFrame {
            code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Away,
            reason: "bye".into(),
        });

        for (sink, session_id) in [(first, "s1"), (second, "s2")] {
            let mut waited = Duration::ZERO;
            while sink.disconnects().is_empty() && waited < Duration::from_secs(5) {
                tokio::time::sleep(Duration::from_millis(20)).await;
                waited += Duration::from_millis(20);
            }
            assert_eq!(sink.disconnects(), [session_id.to_string()]);
        }
    }

    /// The cold-start bug this ack exists for: enqueueing a `Subscribe` proves
    /// only that a process-local channel accepted it. A leg that is a black hole
    /// — established, never aborted, silently carrying nothing — used to leave
    /// `connect` returning `Ok`, the UI saying "connected", and nothing noticing
    /// until `INBOUND_LIVENESS_TIMEOUT` 45s later. It must fail instead.
    /// The ack budget for the two tests whose point is that the budget EXPIRES.
    /// Nothing races it — the server is silent, so however slow the box is, the
    /// timeout is still what fires — which is why it can be this short.
    ///
    /// Real time, not a paused clock: these dial a real loopback socket, and
    /// `start_paused` fast-forwards straight through a pending TCP connect — far
    /// enough to trip `CONNECT_TIMEOUT` before the dial lands.
    const TEST_ACK_BUDGET: Duration = Duration::from_millis(300);

    /// The budget for the one test that needs traffic to BEAT it. A loopback
    /// round trip is microseconds, so this is ~4 orders of magnitude of headroom
    /// — the test must not turn into a stopwatch on a loaded runner.
    const TEST_ACK_BUDGET_BEATABLE: Duration = Duration::from_secs(2);

    #[tokio::test]
    async fn an_unacknowledged_subscribe_fails_instead_of_reporting_connected() {
        let mut server = Server::silent().await;
        let transport = server.transport();
        let registry = SessionRegistry::default().with_subscribe_ack_timeout(TEST_ACK_BUDGET);

        let err = registry
            .connect(&transport, "s1", Arc::new(RecordingSink::default()))
            .await
            .expect_err("an unacknowledged subscribe is not a connection");

        assert!(matches!(err, TransportError::SessionClosed), "got {err:?}");
        assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));
    }

    /// …and the silent leg is RETIRED, not merely failed: every other session on
    /// it is subscribed on a socket that carries nothing, so each one has to be
    /// told to redial. Retiring drops the handle so the pump exits normally and
    /// runs its `on_disconnected` fan-out — an `abort` would strand them.
    #[tokio::test]
    async fn a_silent_leg_is_retired_so_every_session_on_it_redials() {
        let server = Server::silent().await;
        let transport = server.transport();
        let registry = SessionRegistry::default().with_subscribe_ack_timeout(TEST_ACK_BUDGET);
        let bystander = Arc::new(RecordingSink::default());

        // `s2` is already riding the leg when `s1`'s subscribe goes unanswered.
        registry.preconnect(&transport).await.expect("preconnect");
        registry
            .sinks
            .lock()
            .await
            .insert("s2".to_string(), bystander.clone());

        registry
            .connect(&transport, "s1", Arc::new(RecordingSink::default()))
            .await
            .expect_err("unacknowledged");

        let mut waited = Duration::ZERO;
        while bystander.disconnects().is_empty() && waited < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(20)).await;
            waited += Duration::from_millis(20);
        }
        assert_eq!(
            bystander.disconnects(),
            ["s2".to_string()],
            "a bystander on the retired leg must be told to redial"
        );
        assert!(
            !registry.has_live_pump().await,
            "the silent leg must not survive as the live pump"
        );
    }

    /// The other side of that judgement call: when the leg is demonstrably
    /// carrying traffic, an unanswered `Subscribe` is this SESSION's problem (a
    /// cross-channel session, which the gateway answers with a `Notice` and no
    /// bundle). Tearing the leg down there would put every other session through
    /// a redial on its behalf, forever.
    #[tokio::test]
    async fn a_live_leg_survives_one_sessions_unanswered_subscribe() {
        let mut server = Server::silent().await;
        let transport = server.transport();
        let registry =
            SessionRegistry::default().with_subscribe_ack_timeout(TEST_ACK_BUDGET_BEATABLE);

        registry.preconnect(&transport).await.expect("preconnect");
        let connect = {
            let transport = &transport;
            let registry = &registry;
            async move {
                registry
                    .connect(transport, "s1", Arc::new(RecordingSink::default()))
                    .await
            }
        };
        let traffic = async {
            assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));
            // Not the ack — just proof the socket is alive. The Pong is the
            // client confirming it processed the Ping, which is the same poll
            // that restamps `last_inbound`; waiting for it makes the test assert
            // on a fact rather than on a sleep.
            server.send(Frame::Ping);
            assert!(matches!(server.next_frame().await, Frame::Pong));
            tokio::time::sleep(TEST_ACK_BUDGET_BEATABLE).await;
        };
        let (result, ()) = tokio::join!(connect, traffic);

        assert!(
            matches!(result, Err(TransportError::NotConnected)),
            "got {result:?}"
        );
        assert!(
            registry.has_live_pump().await,
            "a leg that is delivering must survive one session's unanswered subscribe"
        );
    }

    /// `HANDSHAKE_REPLY_TIMEOUT` belongs to the handshake CALL SITE, never to
    /// the shared reader. `recv_binary` is also `relay::tunnel::NoiseFrames::recv`
    /// — every REST-over-relay response frame and every blob chunk — where the
    /// budgets are `POOLED_LEG_FIRST_BYTE_TIMEOUT` (15s), `TUNNEL_REQUEST_TIMEOUT`
    /// (30s) and `TUNNEL_HANDSHAKE_TIMEOUT` (15s), all wider than 6s, plus a
    /// 100 MiB upload's post-transfer wait which is deliberately uncapped. Bound
    /// the shared reader and you silently cap all of them.
    ///
    /// The tunnel's own tests drive a `ScriptedFrames` fake and never reach
    /// `recv_binary`, so this is the only place the separation can be seen.
    /// Paused clock is safe here (unlike the registry tests): nothing in this
    /// test wraps a dial in a timeout, so there is no budget for the
    /// auto-advance to consume before the socket is up. A server apiece —
    /// `Server` serves one connection at a time, so a second dial into the same
    /// one would never be accepted.
    #[tokio::test(start_paused = true)]
    async fn only_the_handshake_reader_is_time_bounded() {
        let handshake_server = Server::silent().await;
        let mut ws = dial_raw(handshake_server.addr).await;
        assert!(
            tokio::time::timeout(HANDSHAKE_REPLY_TIMEOUT * 2, recv_binary_handshake(&mut ws))
                .await
                .expect("the handshake reader must give up on its own")
                .is_err(),
            "a peer that accepts and never answers must fail the handshake"
        );

        let tunnel_server = Server::silent().await;
        let mut ws = dial_raw(tunnel_server.addr).await;
        assert!(
            tokio::time::timeout(HANDSHAKE_REPLY_TIMEOUT * 2, recv_binary(&mut ws))
                .await
                .is_err(),
            "the shared reader must still be waiting — a tunnel response or blob \
             chunk is allowed to take longer than a handshake"
        );
    }

    /// A bare client socket, no registry or pump — the two readers under test
    /// take a `WsStream` directly.
    async fn dial_raw(addr: std::net::SocketAddr) -> WsStream {
        let tcp = TcpStream::connect(addr).await.expect("tcp");
        let (ws, _) = client_async(
            format!("ws://{addr}/v1/channel-ws"),
            MaybeTlsStream::Plain(tcp),
        )
        .await
        .expect("ws");
        ws
    }

    /// `send` refuses a session with no registered sink rather than writing into a
    /// leg nobody is listening on.
    #[tokio::test]
    async fn a_send_without_a_subscribed_sink_is_refused() {
        let registry = SessionRegistry::default();

        let err = registry
            .send(
                "s1".to_string(),
                "hi".to_string(),
                "m1".to_string(),
                Vec::new(),
            )
            .await
            .expect_err("must refuse");
        assert!(matches!(err, TransportError::NotConnected));
    }
}
