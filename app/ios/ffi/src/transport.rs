//! The shared chat-transport core: one WebSocket frame pump + a per-leg
//! connection SUPERVISOR that both legs run — the relay (Noise E2E) leg in
//! [`crate::relay::chat`] and the direct (raw-MessagePack) leg in
//! [`crate::direct::chat`]. The two legs are near-identical; they diverge in
//! only two seams, captured by the traits here:
//!
//! * [`FrameCodec`] — how a `Frame` crosses the socket. Relay seals/chunks it in
//!   Noise (1..N binary messages; a decrypt desync is fatal); direct encodes it
//!   1:1 (an unknown future variant is skipped for forward-compat). The fork is
//!   encoded in [`FrameCodec::decode_inbound`]'s return so the pump can treat
//!   `Err` uniformly as "end the session".
//! * [`LegDialer::establish`] — dial + handshake + auth, returning the live
//!   socket already wrapped as a ready [`Connection`]. This is the only divergent
//!   step; the retry/rotation/handshake details stay inside each impl. Owned
//!   (`Arc<dyn LegDialer>`) so the supervisor task can dial without borrowing
//!   the leg.
//!
//! ## The shape: one loop owns every lifecycle decision
//!
//! Connection lifecycle — dialing (coalesced), installing a leg, subscribing
//! sessions, proving subscriptions against the gateway's `SubscribeState` ack,
//! admitting sends, and reporting leg death — lives in ONE supervisor task per
//! registry ([`Supervisor::run`]). Everything reaches it as a [`Msg`] on one
//! unbounded queue: FFI calls (with reply oneshots), the pump's events
//! (`PumpEnded`, `SubscribeAcked`), dial results, and ack timers. Because the
//! loop is the single owner, the invariants that used to be maintained by
//! convention across locks are structural here:
//!
//! * **Leg death is handled exactly once** ([`Supervisor::leg_death`]): it runs
//!   only when the reported `leg_id` matches the current leg, so a duplicate
//!   `PumpEnded`, a death racing a deliberate teardown, or a probe and the
//!   pump's own tail reporting the same corpse all collapse to one transition.
//!   No fences.
//! * **`on_disconnected` is a delivery guarantee**: it is the only thing that
//!   arms the client's redial ladder, and a session that never hears it wedges
//!   on a `connected` it can't leave (the cold-start send black hole of
//!   2026-08-16). Death is discovered on three channels — the pump's tail
//!   event, an enqueue onto a closed pump (covers a panicked pump), and the
//!   health probe in `Preconnect`/`Open` — and all three funnel into the one
//!   transition. The ONE deliberate exception is `Disconnect` (logout/rebind),
//!   which tears the leg down without the fan-out so a teardown doesn't kick
//!   the reconnect ladder against credentials that were just wiped.
//! * **Sends are admitted per-leg**: a session's `Subscribe` is bound to the
//!   `leg_id` it rode; a send is enqueued only while that leg is the live one.
//!   A fresh leg installed by `preconnect` (which subscribes nothing) refuses
//!   the session's sends with `NotConnected` instead of `Ok`-ing them into a
//!   gateway that silently drops not-subscribed messages.
//! * **Acks are leg-scoped**: `SubscribeState` is tagged with the `leg_id` of
//!   the pump it arrived on, so a stale open's ack can never prove (or
//!   clobber) a subscription on a different leg.
//!
//! The HOT PATH deliberately stays out of the loop: the pump routes inbound
//! frames straight to per-session [`FrameSink`]s through the shared routing
//! map, answers the gateway's keepalive `Ping` locally, and stamps the leg's
//! `last_inbound` proof-of-life cell on EVERY socket yield (keepalives
//! included — that stamp is what lets the ack-timeout judgment tell a live leg
//! with one unanswered subscribe from a dead one; see
//! [`Supervisor::ack_timed_out`]). Those two read-mostly surfaces — the
//! routing map (pump reads, supervisor writes) and the per-leg stamp cell
//! (pump writes, supervisor reads) — are the only state shared outside the
//! loop, each with a single writer.

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

/// The stable wire text of [`TransportError::NotConnected`] /
/// [`TransportError::SessionClosed`]. Load-bearing across the FFI boundary:
/// `BayboError::from_msg` matches these exact strings to fold the stringified
/// transport error back into `BayboError::NotConnected`, which Swift's send
/// path uses to fall through to the dial-and-send slow path instead of
/// trusting a stale `connected`.
pub(crate) const NOT_CONNECTED_MSG: &str = "no active session";
pub(crate) const SESSION_CLOSED_MSG: &str = "session closed";

/// One error surface for both legs. Specific variants carry the few cases the
/// shared lifecycle distinguishes; [`TransportError::Other`] carries each leg's
/// own dial/handshake/REST prose verbatim (including the `invalid_token` code
/// that REST returns on a 401, folded into `BayboError::InvalidToken` at the FFI
/// boundary).
#[derive(Debug, thiserror::Error)]
pub(crate) enum TransportError {
    /// A setup / precondition failure — not paired, not signed in, no relay route,
    /// or a keychain read hiccup — surfaced with its own prose so the client can
    /// tell "fix your setup" from "the network ate it".
    #[error("{0}")]
    Precondition(String),
    /// No subscribed session to send on — including a session whose subscription
    /// was proven on a leg that is no longer the live one.
    #[error("{}", NOT_CONNECTED_MSG)]
    NotConnected,
    /// The live session's send half is gone (the pump exited); the next reconnect
    /// re-subscribes.
    #[error("{}", SESSION_CLOSED_MSG)]
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

/// Seam 2: a chat leg's dialer. `establish` is the only divergent step (dial +
/// handshake + auth); the rest of the lifecycle is the shared supervisor below.
/// An OWNED object (`Arc<dyn LegDialer>`, handed to
/// [`SessionRegistry::new`] once) rather than a borrowing trait method: the
/// supervisor task dials from spawned children, which cannot borrow the leg.
/// Both legs' dialers re-read their credentials per call, so the object itself
/// carries no per-connection state.
pub(crate) trait LegDialer: Send + Sync {
    /// Dial, handshake, and authenticate the chat leg, returning the ready
    /// [`Connection`]. Boxed (dyn-compatible) — the dial is seconds of network
    /// I/O, so the allocation is noise.
    fn establish(&self) -> futures_util::future::BoxFuture<'_, Result<Connection, TransportError>>;
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

/// The shared per-session frame-routing map: pump reads on every inbound
/// frame, supervisor writes on open/unsubscribe/disconnect. One of the two
/// deliberate shared surfaces (see the module doc).
type RoutingMap = Arc<Mutex<HashMap<String, Arc<dyn FrameSink>>>>;

/// Everything [`pump`] needs besides the socket: where inbound frames go, the
/// leg's proof-of-life stamp, and the supervisor queue its lifecycle events
/// (`PumpEnded`, `SubscribeAcked`) are reported on, tagged with this pump's
/// `leg_id`.
struct PumpCtx {
    sinks: RoutingMap,
    list_sink: SharedListSink,
    deck_sink: SharedDeckSink,
    last_inbound: Arc<parking_lot::Mutex<Instant>>,
    leg_id: u64,
    events: mpsc::UnboundedSender<Msg>,
}

/// The FFI-facing handle for one leg's connection lifecycle: a thin async
/// front over the supervisor task, plus the two shared read-mostly surfaces
/// the pump touches directly (the routing map and the global sinks).
pub(crate) struct SessionRegistry {
    dialer: Arc<dyn LegDialer>,
    supervisor: std::sync::OnceLock<mpsc::UnboundedSender<Msg>>,
    sinks: RoutingMap,
    /// Connection-global `SessionActivity` pings (chat-list unread) land here.
    list_sink: SharedListSink,
    /// Connection-global deck pushes (`DeckCardData` / `DeckChanged`) land here.
    deck_sink: SharedDeckSink,
    /// [`SUBSCRIBE_ACK_TIMEOUT`], injectable so the tests that must sit out an
    /// unanswered subscribe cost milliseconds instead of seconds. Read at
    /// supervisor spawn, so a test override must precede the first operation.
    subscribe_ack_timeout: Duration,
}

impl SessionRegistry {
    pub(crate) fn new(dialer: Arc<dyn LegDialer>) -> Self {
        Self {
            dialer,
            supervisor: std::sync::OnceLock::new(),
            sinks: Arc::new(Mutex::new(HashMap::new())),
            list_sink: Arc::new(parking_lot::Mutex::new(None)),
            deck_sink: Arc::new(parking_lot::Mutex::new(None)),
            subscribe_ack_timeout: SUBSCRIBE_ACK_TIMEOUT,
        }
    }
}

impl SessionRegistry {
    /// Shrink the subscribe-ack budget so a test can exercise the unanswered
    /// path without sleeping through the production one. Must run before the
    /// first operation — the supervisor reads the budget once at spawn.
    #[cfg(test)]
    fn with_subscribe_ack_timeout(mut self, budget: Duration) -> Self {
        self.subscribe_ack_timeout = budget;
        self
    }

    /// The supervisor's queue, spawning the loop on first use. Lazy because
    /// the registry is built in sync constructors before any runtime context
    /// exists; every caller of this is already on the core runtime.
    fn supervisor(&self) -> &mpsc::UnboundedSender<Msg> {
        self.supervisor.get_or_init(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            let supervisor = Supervisor {
                dialer: self.dialer.clone(),
                sinks: self.sinks.clone(),
                list_sink: self.list_sink.clone(),
                deck_sink: self.deck_sink.clone(),
                ack_budget: self.subscribe_ack_timeout,
                tx: tx.clone(),
                leg: Leg::Idle,
                sessions: HashMap::new(),
                next_id: 0,
            };
            tokio::spawn(supervisor.run(rx));
            tx
        })
    }

    /// One request/reply round trip to the supervisor. A closed queue (the
    /// loop is process-lifetime, so this means the process is tearing down)
    /// degrades to `NotConnected` rather than hanging the FFI call.
    async fn request(&self, build: impl FnOnce(Reply) -> Msg) -> Result<(), TransportError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self.supervisor().send(build(reply_tx)).is_err() {
            return Err(TransportError::NotConnected);
        }
        reply_rx.await.map_err(|_| TransportError::NotConnected)?
    }

    /// Open the binding's global chat leg without subscribing any session. Used
    /// to warm the relay content leg at app launch so the first chat screen only
    /// needs to enqueue `Subscribe`.
    pub(crate) async fn preconnect(&self) -> Result<(), TransportError> {
        self.request(|reply| Msg::Preconnect { reply }).await
    }

    /// Subscribe `session_id` on the binding's global chat leg, streaming that
    /// session's frames to `sink`. Returns once the gateway has ACKNOWLEDGED
    /// the subscription with its `SubscribeState` bundle — enqueueing is not
    /// connecting.
    pub(crate) async fn connect(
        &self,
        session_id: &str,
        sink: Arc<dyn FrameSink>,
    ) -> Result<(), TransportError> {
        self.request(|reply| Msg::Open {
            session_id: session_id.to_string(),
            sink,
            message: None,
            reply,
        })
        .await
    }

    /// Subscribe `session_id` and enqueue the first user message on the same
    /// outbound pump, preserving Subscribe-before-Message ordering for draft
    /// sessions that do not have a live subscription yet.
    pub(crate) async fn connect_and_send(
        &self,
        session_id: &str,
        sink: Arc<dyn FrameSink>,
        message: OutboundMessage,
    ) -> Result<(), TransportError> {
        self.request(|reply| Msg::Open {
            session_id: session_id.to_string(),
            sink,
            message: Some(message),
            reply,
        })
        .await
    }

    /// Queue a user message on the live session for the pump to build + seal +
    /// send. Admitted only while the live leg IS the one this session's
    /// `Subscribe` rode; see [`Supervisor::send_user_message`].
    pub(crate) async fn send(
        &self,
        session_id: String,
        text: String,
        msg_id: String,
        attachments: Vec<WireAttachment>,
    ) -> Result<(), TransportError> {
        self.request(|reply| Msg::Send {
            session_id,
            text,
            msg_id,
            attachments,
            reply,
        })
        .await
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
        self.request(|reply| Msg::ResolveApproval {
            call_id,
            decision,
            reply,
        })
        .await
    }

    /// Tear down the live pump (or the dial in flight) and drop every session.
    /// The ONE teardown that suppresses `on_disconnected`: logout/rebind must
    /// not kick the reconnect ladder against credentials that were just wiped.
    pub(crate) async fn disconnect(&self) {
        let _ = self.request(|reply| Msg::Disconnect { reply }).await;
    }

    /// Drop `session_id`'s sink from the global leg WITHOUT tearing the leg
    /// down — the client's LRU eviction of an idle, offscreen session. There
    /// is no wire-level unsubscribe, so the gateway keeps the subscription
    /// until the leg next cycles; an evicted session simply stops receiving.
    pub(crate) async fn unsubscribe(&self, session_id: &str) {
        let _ = self
            .request(|reply| Msg::Unsubscribe {
                session_id: session_id.to_string(),
                reply,
            })
            .await;
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

/// A supervisor request's reply channel. Dropped receivers (a cancelled FFI
/// call) make the send a no-op.
type Reply = oneshot::Sender<Result<(), TransportError>>;

/// Everything that reaches the supervisor loop: FFI operations (each carrying
/// its reply) and internal lifecycle events from the pump, the dial child, and
/// the ack timers.
enum Msg {
    Open {
        session_id: String,
        sink: Arc<dyn FrameSink>,
        message: Option<OutboundMessage>,
        reply: Reply,
    },
    Send {
        session_id: String,
        text: String,
        msg_id: String,
        attachments: Vec<WireAttachment>,
        reply: Reply,
    },
    ResolveApproval {
        call_id: String,
        decision: WireApprovalDecision,
        reply: Reply,
    },
    Preconnect {
        reply: Reply,
    },
    Disconnect {
        reply: Reply,
    },
    Unsubscribe {
        session_id: String,
        reply: Reply,
    },
    /// The dial child finished; `leg_id` names the dial it answers. The
    /// connection is boxed so this rare variant doesn't size every message on
    /// the queue.
    DialFinished {
        leg_id: u64,
        result: Result<Box<Connection>, TransportError>,
    },
    /// The pump's own tail: its socket ended (peer close, read error, liveness
    /// lapse, Noise desync). An aborted pump never sends this — the aborter
    /// owns the transition instead.
    PumpEnded {
        leg_id: u64,
    },
    /// The gateway acknowledged a `Subscribe`: a `SubscribeState` arrived on
    /// the pump with this `leg_id`. Leg-tagged by construction (the tag is the
    /// receiving pump's), so a stale attempt's ack can never prove a
    /// subscription on a different leg.
    SubscribeAcked {
        leg_id: u64,
        session_id: String,
    },
    /// A `Subscribe`'s ack budget lapsed. `attempt` fences it to the exact
    /// park it was armed for — a session re-opened on the same leg since must
    /// not be failed by the previous attempt's timer.
    AckTimedOut {
        leg_id: u64,
        session_id: String,
        attempt: u64,
        parked_at: Instant,
    },
    /// Abort the live pump WITHOUT the death transition — a stand-in for a
    /// panicked pump / a corpse in the probe window, so tests can pin that the
    /// other discovery channels still deliver `on_disconnected`.
    #[cfg(test)]
    AbortPumpForTest {
        reply: Reply,
    },
    /// Install a session as already-Proven on the current live leg — a
    /// bystander riding the leg without running a full `connect` (the server
    /// may be deliberately silent).
    #[cfg(test)]
    InjectProvenForTest {
        session_id: String,
        sink: Arc<dyn FrameSink>,
        reply: Reply,
    },
}

/// A caller parked on a dial in flight.
enum DialWaiter {
    /// `preconnect`: wants a live leg, subscribes nothing.
    Warm { reply: Reply },
    /// An `Open`: once the leg is up, its `Subscribe` (+ optional first
    /// message) is enqueued and the session parks for the ack.
    Open {
        session_id: String,
        message: Option<OutboundMessage>,
        reply: Reply,
    },
}

impl DialWaiter {
    fn fail(self, err: TransportError) {
        let reply = match self {
            DialWaiter::Warm { reply } => reply,
            DialWaiter::Open { reply, .. } => reply,
        };
        let _ = reply.send(Err(err));
    }
}

/// The one leg the registry runs, as the supervisor sees it.
enum Leg {
    Idle,
    /// A dial child is out. `adopters` were present when it started and share
    /// its failure; `latecomers` arrived during it and — mirroring the old
    /// behavior where each open ran its own establish after the lock — get a
    /// fresh dial if this one fails.
    Dialing {
        leg_id: u64,
        adopters: Vec<DialWaiter>,
        latecomers: Vec<DialWaiter>,
    },
    Live {
        leg_id: u64,
        outbound_tx: mpsc::UnboundedSender<OutboundCmd>,
        task: tokio::task::JoinHandle<()>,
        /// The leg's proof-of-life cell, written by the pump on EVERY socket
        /// yield (keepalives included) and read by the ack-timeout judgment.
        last_inbound: Arc<parking_lot::Mutex<Instant>>,
    },
}

/// One session's lifecycle state. The sink here is the one the LATEST open
/// installed — routing and death attribution switch to it atomically inside
/// the `Open` handler, so a death report can never reach a dial that no longer
/// rides the dead leg.
struct SessionState {
    sink: Arc<dyn FrameSink>,
    phase: Phase,
}

enum Phase {
    /// A sink is installed (frames still route) but no subscription is live or
    /// in flight: after a failed open, or after the leg it rode died.
    Registered,
    /// A `Subscribe` is on leg `on_leg`'s wire, waiting for the gateway's
    /// `SubscribeState`. Sends are already admitted — the socket is serial, so
    /// they land behind the `Subscribe`.
    Subscribing {
        on_leg: u64,
        attempt: u64,
        waiters: Vec<Reply>,
    },
    /// The gateway acknowledged the subscription on leg `on_leg`.
    Proven { on_leg: u64 },
}

impl Phase {
    /// The leg this session's subscription (proven or in flight) rides.
    fn rides(&self) -> Option<u64> {
        match self {
            Phase::Registered => None,
            Phase::Subscribing { on_leg, .. } | Phase::Proven { on_leg } => Some(*on_leg),
        }
    }
}

/// The single owner of one leg's connection lifecycle. Runs as one task per
/// registry, processing [`Msg`]s serially; see the module doc for the
/// invariants this buys. The loop body never awaits foreign code and never
/// panics — it is the delivery guarantee for `on_disconnected`.
struct Supervisor {
    dialer: Arc<dyn LegDialer>,
    sinks: RoutingMap,
    list_sink: SharedListSink,
    deck_sink: SharedDeckSink,
    ack_budget: Duration,
    /// Self-sender for dial children, pumps, and ack timers.
    tx: mpsc::UnboundedSender<Msg>,
    leg: Leg,
    sessions: HashMap<String, SessionState>,
    /// One counter for both leg ids and subscribe attempts — only uniqueness
    /// matters.
    next_id: u64,
}

impl Supervisor {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        while let Some(msg) = rx.recv().await {
            self.handle(msg).await;
        }
    }

    async fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Open {
                session_id,
                sink,
                message,
                reply,
            } => self.open(session_id, sink, message, reply).await,
            Msg::Send {
                session_id,
                text,
                msg_id,
                attachments,
                reply,
            } => self.send_user_message(session_id, text, msg_id, attachments, reply),
            Msg::ResolveApproval {
                call_id,
                decision,
                reply,
            } => self.resolve_approval(call_id, decision, reply),
            Msg::Preconnect { reply } => self.preconnect(reply),
            Msg::Disconnect { reply } => self.disconnect(reply).await,
            Msg::Unsubscribe { session_id, reply } => {
                self.sessions.remove(&session_id);
                self.sinks.lock().await.remove(&session_id);
                let _ = reply.send(Ok(()));
            }
            Msg::DialFinished { leg_id, result } => self.dial_finished(leg_id, result).await,
            Msg::PumpEnded { leg_id } => self.leg_death(leg_id),
            Msg::SubscribeAcked { leg_id, session_id } => self.subscribe_acked(leg_id, session_id),
            Msg::AckTimedOut {
                leg_id,
                session_id,
                attempt,
                parked_at,
            } => self.ack_timed_out(leg_id, session_id, attempt, parked_at),
            #[cfg(test)]
            Msg::AbortPumpForTest { reply } => {
                if let Leg::Live { task, .. } = &self.leg {
                    task.abort();
                }
                let _ = reply.send(Ok(()));
            }
            #[cfg(test)]
            Msg::InjectProvenForTest {
                session_id,
                sink,
                reply,
            } => {
                let Leg::Live { leg_id, .. } = &self.leg else {
                    let _ = reply.send(Err(TransportError::NotConnected));
                    return;
                };
                let on_leg = *leg_id;
                self.sinks
                    .lock()
                    .await
                    .insert(session_id.clone(), sink.clone());
                self.sessions.insert(
                    session_id,
                    SessionState {
                        sink,
                        phase: Phase::Proven { on_leg },
                    },
                );
                let _ = reply.send(Ok(()));
            }
        }
    }

    /// An open supersedes anything the session had in flight: routing and
    /// death attribution move to the new sink FIRST, and the old phase's
    /// waiters are failed rather than left to a timer. The withdraw also
    /// un-binds the session from the current leg, so a corpse discovered by
    /// the enqueue below is announced to everyone EXCEPT this session — its
    /// own dial is the fresher verdict.
    async fn open(
        &mut self,
        session_id: String,
        sink: Arc<dyn FrameSink>,
        message: Option<OutboundMessage>,
        reply: Reply,
    ) {
        self.sinks
            .lock()
            .await
            .insert(session_id.clone(), sink.clone());
        let prior = self.sessions.insert(
            session_id.clone(),
            SessionState {
                sink,
                phase: Phase::Registered,
            },
        );
        if let Some(SessionState {
            phase: Phase::Subscribing { waiters, .. },
            ..
        }) = prior
        {
            for waiter in waiters {
                let _ = waiter.send(Err(TransportError::NotConnected));
            }
        }

        let (leg_id, outbound_tx) = match &mut self.leg {
            Leg::Live {
                leg_id,
                outbound_tx,
                ..
            } => (*leg_id, outbound_tx.clone()),
            Leg::Dialing { latecomers, .. } => {
                latecomers.push(DialWaiter::Open {
                    session_id,
                    message,
                    reply,
                });
                return;
            }
            Leg::Idle => {
                self.start_dial(vec![DialWaiter::Open {
                    session_id,
                    message,
                    reply,
                }]);
                return;
            }
        };
        if enqueue_open(&outbound_tx, &session_id, message.as_ref()) {
            self.park_subscribing(session_id, leg_id, reply);
        } else {
            self.leg_death(leg_id);
            self.start_dial(vec![DialWaiter::Open {
                session_id,
                message,
                reply,
            }]);
        }
    }

    /// Park an open whose `Subscribe` is on `leg_id`'s wire, and arm its ack
    /// timer. The `attempt` fences the timer to this exact park.
    fn park_subscribing(&mut self, session_id: String, leg_id: u64, reply: Reply) {
        self.next_id += 1;
        let attempt = self.next_id;
        let parked_at = Instant::now();
        let Some(session) = self.sessions.get_mut(&session_id) else {
            // `open` inserted the entry moments ago; its absence means an
            // interleaved teardown drained the map — fail rather than hang.
            let _ = reply.send(Err(TransportError::SessionClosed));
            return;
        };
        session.phase = Phase::Subscribing {
            on_leg: leg_id,
            attempt,
            waiters: vec![reply],
        };
        let tx = self.tx.clone();
        let budget = self.ack_budget;
        tokio::spawn(async move {
            tokio::time::sleep(budget).await;
            let _ = tx.send(Msg::AckTimedOut {
                leg_id,
                session_id,
                attempt,
                parked_at,
            });
        });
    }

    /// The send gate: enqueue only while the live leg IS the leg this
    /// session's `Subscribe` rode ([`Phase::rides`]). "A sink exists and a
    /// pump is alive" is not enough: after a leg death, a fresh leg installed
    /// by `preconnect` carries no subscriptions, and a send `Ok`'d onto it is
    /// silently dropped by the gateway as not-subscribed — the client-visible
    /// shape is a spinner that never resolves. Refusing surfaces
    /// `NotConnected`, which the client answers by re-dialing (subscribe +
    /// send on the current leg).
    fn send_user_message(
        &mut self,
        session_id: String,
        text: String,
        msg_id: String,
        attachments: Vec<WireAttachment>,
        reply: Reply,
    ) {
        let Leg::Live {
            leg_id,
            outbound_tx,
            ..
        } = &self.leg
        else {
            let _ = reply.send(Err(TransportError::NotConnected));
            return;
        };
        let leg_id = *leg_id;
        let admitted = self
            .sessions
            .get(&session_id)
            .is_some_and(|session| session.phase.rides() == Some(leg_id));
        if !admitted {
            let _ = reply.send(Err(TransportError::NotConnected));
            return;
        }
        let sent = outbound_tx
            .send(OutboundCmd::Send {
                session_id,
                text,
                msg_id,
                attachments,
            })
            .is_ok();
        if sent {
            let _ = reply.send(Ok(()));
        } else {
            self.leg_death(leg_id);
            let _ = reply.send(Err(TransportError::NotConnected));
        }
    }

    fn resolve_approval(&mut self, call_id: String, decision: WireApprovalDecision, reply: Reply) {
        let Leg::Live {
            leg_id,
            outbound_tx,
            ..
        } = &self.leg
        else {
            let _ = reply.send(Err(TransportError::NotConnected));
            return;
        };
        let leg_id = *leg_id;
        let sent = outbound_tx
            .send(OutboundCmd::ResolveApproval { call_id, decision })
            .is_ok();
        if sent {
            let _ = reply.send(Ok(()));
        } else {
            self.leg_death(leg_id);
            let _ = reply.send(Err(TransportError::NotConnected));
        }
    }

    fn preconnect(&mut self, reply: Reply) {
        let dead = match &mut self.leg {
            Leg::Live {
                leg_id,
                outbound_tx,
                task,
                ..
            } => {
                if !outbound_tx.is_closed() && !task.is_finished() {
                    let _ = reply.send(Ok(()));
                    return;
                }
                *leg_id
            }
            Leg::Dialing { latecomers, .. } => {
                latecomers.push(DialWaiter::Warm { reply });
                return;
            }
            Leg::Idle => {
                self.start_dial(vec![DialWaiter::Warm { reply }]);
                return;
            }
        };
        // The probe found a corpse (a pump that died without getting its tail
        // event out — e.g. aborted mid-poll, or panicked): the third death-
        // discovery channel.
        self.leg_death(dead);
        self.start_dial(vec![DialWaiter::Warm { reply }]);
    }

    fn start_dial(&mut self, adopters: Vec<DialWaiter>) {
        self.next_id += 1;
        let leg_id = self.next_id;
        self.leg = Leg::Dialing {
            leg_id,
            adopters,
            latecomers: Vec::new(),
        };
        let dialer = self.dialer.clone();
        // Send-on-drop: `DialFinished` is the ONLY exit from `Leg::Dialing`,
        // and its waiters' replies are HELD in the leg — a dial child that
        // dies without reporting (a panic inside `establish`'s foreign dial
        // stack, which has happened before: a mis-featured rustls once
        // panicked building its ClientConfig; or runtime-shutdown
        // cancellation) would otherwise strand the supervisor in `Dialing`
        // and hang every parked and future connect until app relaunch.
        struct DialReport {
            tx: mpsc::UnboundedSender<Msg>,
            leg_id: u64,
            armed: bool,
        }
        impl DialReport {
            fn finish(mut self, result: Result<Box<Connection>, TransportError>) {
                self.armed = false;
                let _ = self.tx.send(Msg::DialFinished {
                    leg_id: self.leg_id,
                    result,
                });
            }
        }
        impl Drop for DialReport {
            fn drop(&mut self) {
                if self.armed {
                    let _ = self.tx.send(Msg::DialFinished {
                        leg_id: self.leg_id,
                        result: Err(TransportError::Other(
                            "dial task died before reporting".into(),
                        )),
                    });
                }
            }
        }
        let report = DialReport {
            tx: self.tx.clone(),
            leg_id,
            armed: true,
        };
        tokio::spawn(async move {
            let result = match tokio::time::timeout(CONNECT_TIMEOUT, dialer.establish()).await {
                Ok(result) => result.map(Box::new),
                Err(_) => Err(TransportError::Timeout),
            };
            report.finish(result);
        });
    }

    async fn dial_finished(
        &mut self,
        leg_id: u64,
        result: Result<Box<Connection>, TransportError>,
    ) {
        let (adopters, latecomers) = match &mut self.leg {
            Leg::Dialing {
                leg_id: current,
                adopters,
                latecomers,
            } if *current == leg_id => (std::mem::take(adopters), std::mem::take(latecomers)),
            _ => {
                // Superseded by a teardown mid-dial: the connection must not
                // survive as an orphan socket. Closed on its own task — the
                // close is a network write with no deadline, and the loop
                // never awaits foreign I/O.
                if let Ok(conn) = result {
                    tokio::spawn(async move {
                        let mut ws = conn.ws;
                        let _ = ws.close(None).await;
                    });
                    log::info!("dial discarded: superseded by a teardown mid-dial");
                }
                return;
            }
        };
        match result {
            Ok(conn) => {
                let outbound_tx = self.install_pump(leg_id, *conn);
                let mut waiters = adopters.into_iter().chain(latecomers);
                while let Some(waiter) = waiters.next() {
                    match waiter {
                        DialWaiter::Warm { reply } => {
                            let _ = reply.send(Ok(()));
                        }
                        DialWaiter::Open {
                            session_id,
                            message,
                            reply,
                        } => {
                            // The session may have been withdrawn (LRU
                            // unsubscribe, disconnect) while this open was
                            // parked on the dial — nothing may reach the wire
                            // for it.
                            if !self.sessions.contains_key(&session_id) {
                                let _ = reply.send(Err(TransportError::SessionClosed));
                                continue;
                            }
                            if enqueue_open(&outbound_tx, &session_id, message.as_ref()) {
                                self.park_subscribing(session_id, leg_id, reply);
                            } else {
                                // The freshly-installed pump is already a
                                // corpse (it died without its tail — e.g. a
                                // panic mid-poll). Same funnel as every other
                                // enqueue failure: the death transition, then
                                // ONE fresh dial for this waiter and everyone
                                // still undrained.
                                self.leg_death(leg_id);
                                let mut redial = vec![DialWaiter::Open {
                                    session_id,
                                    message,
                                    reply,
                                }];
                                redial.extend(waiters);
                                self.start_dial(redial);
                                return;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                log::warn!("chat dial failed: {e}");
                for waiter in adopters {
                    waiter.fail(clone_for_waiter(&e));
                }
                if latecomers.is_empty() {
                    self.leg = Leg::Idle;
                } else {
                    // Arrivals during the failed dial share ONE fresh attempt
                    // (per-batch retry); recovery beyond that is the client's
                    // redial ladder.
                    self.start_dial(latecomers);
                }
            }
        }
    }

    fn install_pump(
        &mut self,
        leg_id: u64,
        conn: Connection,
    ) -> mpsc::UnboundedSender<OutboundCmd> {
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
                last_inbound: last_inbound.clone(),
                leg_id,
                events: self.tx.clone(),
            },
            outbound_rx,
        ));
        self.leg = Leg::Live {
            leg_id,
            outbound_tx: outbound_tx.clone(),
            task,
            last_inbound,
        };
        outbound_tx
    }

    /// THE leg-death transition — the delivery guarantee behind
    /// `on_disconnected` (see the module doc). A no-op unless `leg_id` is the
    /// current live leg, which is what makes every discovery channel (pump
    /// tail, failed enqueue, probe) and every duplicate report collapse to one
    /// transition. Sessions riding the leg hear the death through the sink
    /// their subscription rode; parked opens fail `SessionClosed`.
    fn leg_death(&mut self, leg_id: u64) {
        let ours = matches!(&self.leg, Leg::Live { leg_id: current, .. } if *current == leg_id);
        if !ours {
            return;
        }
        if let Leg::Live { task, .. } = std::mem::replace(&mut self.leg, Leg::Idle) {
            task.abort();
        }
        for (session_id, session) in self.sessions.iter_mut() {
            if session.phase.rides() != Some(leg_id) {
                continue;
            }
            let prior = std::mem::replace(&mut session.phase, Phase::Registered);
            if let Phase::Subscribing { waiters, .. } = prior {
                for waiter in waiters {
                    let _ = waiter.send(Err(TransportError::SessionClosed));
                }
            }
            session.sink.on_disconnected(session_id.clone());
        }
    }

    fn subscribe_acked(&mut self, leg_id: u64, session_id: String) {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let Phase::Subscribing {
            on_leg, waiters, ..
        } = &mut session.phase
        else {
            // Re-subscribes of a Proven session ack again; nothing waits.
            return;
        };
        if *on_leg != leg_id {
            return;
        }
        let waiters = std::mem::take(waiters);
        session.phase = Phase::Proven { on_leg: leg_id };
        for waiter in waiters {
            let _ = waiter.send(Ok(()));
        }
    }

    /// A `Subscribe`'s ack budget lapsed. The judgment (scar of 84667591) has
    /// to tell two very different failures apart off the leg's proof-of-life
    /// cell:
    ///
    /// * The leg delivered NOTHING while the subscribe waited — it is a black
    ///   hole (a half-open socket, typically one dialed as the radio came up
    ///   and then silently orphaned). Retire it via the death transition so
    ///   every session on it redials.
    /// * The leg kept delivering (keepalives count — the cell is stamped on
    ///   every socket yield) — it is healthy and this one subscribe was
    ///   rejected or lost (e.g. a session scoped to another channel, which the
    ///   gateway answers with a `Notice` and no bundle). Fail this open only;
    ///   tearing the leg down would put every other session through a redial
    ///   on someone else's behalf, forever.
    fn ack_timed_out(&mut self, leg_id: u64, session_id: String, attempt: u64, parked_at: Instant) {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let fenced = matches!(
            &session.phase,
            Phase::Subscribing { on_leg, attempt: parked, .. }
                if *on_leg == leg_id && *parked == attempt
        );
        if !fenced {
            return;
        }
        let Phase::Subscribing { waiters, .. } =
            std::mem::replace(&mut session.phase, Phase::Registered)
        else {
            return;
        };

        let leg_live = matches!(
            &self.leg,
            Leg::Live { leg_id: current, last_inbound, .. }
                if *current == leg_id && *last_inbound.lock() > parked_at
        );
        if leg_live {
            log::warn!(
                "subscribe session={session_id} unacknowledged after {:?}, but the leg is live; not touching it",
                self.ack_budget
            );
            for waiter in waiters {
                let _ = waiter.send(Err(TransportError::NotConnected));
            }
        } else {
            log::warn!(
                "subscribe session={session_id} unacknowledged after {:?} and the leg carried nothing; retiring it",
                self.ack_budget
            );
            for waiter in waiters {
                let _ = waiter.send(Err(TransportError::SessionClosed));
            }
            self.leg_death(leg_id);
        }
    }

    async fn disconnect(&mut self, reply: Reply) {
        match std::mem::replace(&mut self.leg, Leg::Idle) {
            // Deliberate teardown: abort WITHOUT the death transition, so no
            // on_disconnected fires (the logout contract). A late PumpEnded
            // from this leg no-ops on the leg_id mismatch.
            Leg::Live { task, .. } => task.abort(),
            Leg::Dialing {
                adopters,
                latecomers,
                ..
            } => {
                for waiter in adopters.into_iter().chain(latecomers) {
                    waiter.fail(TransportError::SessionClosed);
                }
                // The dial child's DialFinished will find its leg_id stale and
                // close the socket.
            }
            Leg::Idle => {}
        }
        for (_, session) in self.sessions.drain() {
            if let Phase::Subscribing { waiters, .. } = session.phase {
                for waiter in waiters {
                    let _ = waiter.send(Err(TransportError::SessionClosed));
                }
            }
        }
        self.sinks.lock().await.clear();
        let _ = reply.send(Ok(()));
    }
}

/// Enqueue an open's `Subscribe` (+ optional first message) on a live pump,
/// preserving Subscribe-before-Message ordering on the serial socket. `false`
/// means the pump is dead (its receiver is gone). The invariant callers rely
/// on is ordering-shaped: the user Message can never precede or outlive its
/// `Subscribe` on this leg — a `Subscribe` alone may already be on the dying
/// wire, which is harmless (the leg is being retired, the retried open
/// re-sends both, and the gateway dedups the resent Message on
/// `platform_msg_id`).
fn enqueue_open(
    outbound_tx: &mpsc::UnboundedSender<OutboundCmd>,
    session_id: &str,
    message: Option<&OutboundMessage>,
) -> bool {
    let mut cmds = vec![OutboundCmd::Subscribe {
        session_id: session_id.to_string(),
    }];
    if let Some(message) = message {
        cmds.push(OutboundCmd::Send {
            session_id: session_id.to_string(),
            text: message.text.clone(),
            msg_id: message.msg_id.clone(),
            attachments: message.attachments.clone(),
        });
    }
    for cmd in cmds {
        if outbound_tx.send(cmd).is_err() {
            return false;
        }
    }
    true
}

/// A dial failure is delivered to every adopter, but [`TransportError`] is not
/// `Clone` (it carries a [`MobileError`]); reconstruct an equivalent per
/// waiter, stringifying the one non-trivial variant.
fn clone_for_waiter(e: &TransportError) -> TransportError {
    match e {
        TransportError::Precondition(msg) => TransportError::Precondition(msg.clone()),
        TransportError::NotConnected => TransportError::NotConnected,
        TransportError::SessionClosed => TransportError::SessionClosed,
        TransportError::Timeout => TransportError::Timeout,
        TransportError::Codec(err) => TransportError::Other(err.to_string()),
        TransportError::Other(msg) => TransportError::Other(msg.clone()),
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

/// A chat leg, seen as "the thing that owns a [`SessionRegistry`]". Exposing the
/// registry through one trait lets the session fns below
/// ([`connect`]/[`send`]/[`disconnect`]) drive either leg, so
/// neither `RelaySessions` nor `DirectSessions` re-declares the same
/// delegating wrappers. The dial seam ([`LegDialer`]) is handed to the
/// registry once at construction, so nothing here needs it.
pub(crate) trait SessionLeg {
    fn registry(&self) -> &SessionRegistry;
}

/// Open `leg`'s global chat connection without subscribing any session.
/// Stringifies the error for the FFI boundary.
pub(crate) async fn preconnect<L: SessionLeg>(leg: &L) -> Result<(), String> {
    leg.registry().preconnect().await.map_err(|e| e.to_string())
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
        .connect(&session_id, sink)
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
        .connect_and_send(&session_id, sink, message)
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
/// Returning means the socket ended on its own; the tail reports it to the
/// supervisor, whose death transition delivers `on_disconnected`. An ABORTED
/// pump (supervisor teardown, or a discovered corpse) never reaches the tail —
/// by design: the aborter owns the transition, and a late `PumpEnded` from a
/// leg that is no longer current is a no-op there.
async fn pump(conn: Connection, ctx: PumpCtx, outbound_rx: mpsc::UnboundedReceiver<OutboundCmd>) {
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

async fn dispatch_inbound_frame(ctx: &PumpCtx, frame: Frame) {
    let PumpCtx {
        sinks,
        list_sink,
        deck_sink,
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
                        last_inbound: Arc::new(parking_lot::Mutex::new(Instant::now())),
                        leg_id: 1,
                        events: mpsc::unbounded_channel().0,
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

    /// The loopback leg: a real WebSocket on 127.0.0.1 speaking the direct leg's
    /// raw-MessagePack codec, so the supervisor + pump under test are the
    /// production ones and only the dial is local.
    struct LoopbackDialer {
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

    impl LegDialer for LoopbackDialer {
        fn establish(
            &self,
        ) -> futures_util::future::BoxFuture<'_, Result<Connection, TransportError>> {
            Box::pin(async move {
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
            })
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

        /// A registry dialing this server, plus the dial counter the tests
        /// that assert socket reuse read.
        fn registry(&self) -> (SessionRegistry, Arc<AtomicUsize>) {
            let dials = Arc::new(AtomicUsize::new(0));
            let registry = SessionRegistry::new(Arc::new(LoopbackDialer {
                addr: self.addr,
                dials: dials.clone(),
            }));
            (registry, dials)
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
        let (registry, _dials) = server.registry();
        let sink = Arc::new(RecordingSink::default());

        registry
            .connect_and_send(
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
        let (registry, dials) = server.registry();

        registry.preconnect().await.expect("preconnect");

        assert!(
            server.stayed_quiet(Duration::from_millis(200)).await,
            "preconnect must not send a Subscribe"
        );

        registry
            .connect("s1", Arc::new(RecordingSink::default()))
            .await
            .expect("connect");
        assert!(matches!(
            server.next_frame().await,
            Frame::Subscribe { session_id } if session_id.as_str() == "s1"
        ));
        assert_eq!(
            dials.load(Ordering::Relaxed),
            1,
            "opening a session must reuse the warmed leg, not redial"
        );
    }

    /// One leg carries many sessions: subscribing a second session sends another
    /// Subscribe on the SAME socket and leaves the first subscription alone.
    #[tokio::test]
    async fn switching_sessions_reuses_the_one_leg() {
        let mut server = Server::start().await;
        let (registry, dials) = server.registry();

        registry
            .connect("s1", Arc::new(RecordingSink::default()))
            .await
            .expect("connect s1");
        registry
            .connect("s2", Arc::new(RecordingSink::default()))
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
            dials.load(Ordering::Relaxed),
            1,
            "a second session must not open a second socket"
        );
    }

    /// The gateway's application keepalive is answered locally and NEVER forwarded:
    /// a `Ping` reaching the transcript would render as an unknown frame.
    #[tokio::test]
    async fn a_ping_is_answered_with_a_pong_and_never_forwarded_to_a_sink() {
        let mut server = Server::start().await;
        let (registry, _dials) = server.registry();
        let sink = Arc::new(RecordingSink::default());

        registry.connect("s1", sink.clone()).await.expect("connect");
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
        let (registry, _dials) = server.registry();
        let sink = Arc::new(RecordingSink::default());

        registry.connect("s1", sink.clone()).await.expect("connect");
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
        let (registry, _dials) = server.registry();
        let first = Arc::new(RecordingSink::default());
        let second = Arc::new(RecordingSink::default());

        registry
            .connect("s1", first.clone())
            .await
            .expect("connect s1");
        registry
            .connect("s2", second.clone())
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
        let (registry, _dials) = server.registry();
        let registry = registry.with_subscribe_ack_timeout(TEST_ACK_BUDGET);

        let err = registry
            .connect("s1", Arc::new(RecordingSink::default()))
            .await
            .expect_err("an unacknowledged subscribe is not a connection");

        assert!(matches!(err, TransportError::SessionClosed), "got {err:?}");
        assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));
    }

    /// …and the silent leg is RETIRED, not merely failed: every other session on
    /// it is subscribed on a socket that carries nothing, so each one has to be
    /// told to redial through the death transition.
    #[tokio::test]
    async fn a_silent_leg_is_retired_so_every_session_on_it_redials() {
        let server = Server::silent().await;
        let (registry, _dials) = server.registry();
        let registry = registry.with_subscribe_ack_timeout(TEST_ACK_BUDGET);
        let bystander = Arc::new(RecordingSink::default());

        // `s2` is already riding the leg when `s1`'s subscribe goes unanswered.
        registry.preconnect().await.expect("preconnect");
        ride_leg(&registry, "s2", bystander.clone()).await;

        registry
            .connect("s1", Arc::new(RecordingSink::default()))
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
    }

    /// The other side of that judgement call: when the leg is demonstrably
    /// carrying traffic, an unanswered `Subscribe` is this SESSION's problem (a
    /// cross-channel session, which the gateway answers with a `Notice` and no
    /// bundle). Tearing the leg down there would put every other session through
    /// a redial on its behalf, forever.
    #[tokio::test]
    async fn a_live_leg_survives_one_sessions_unanswered_subscribe() {
        let mut server = Server::silent().await;
        let (registry, dials) = server.registry();
        let registry = registry.with_subscribe_ack_timeout(TEST_ACK_BUDGET_BEATABLE);

        registry.preconnect().await.expect("preconnect");
        let connect = {
            let registry = &registry;
            async move {
                registry
                    .connect("s1", Arc::new(RecordingSink::default()))
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
        // The spared leg is still the live one: a later probe reuses it
        // instead of redialing.
        registry.preconnect().await.expect("preconnect probe");
        assert_eq!(
            dials.load(Ordering::Relaxed),
            1,
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
        let server = Server::start().await;
        let (registry, _dials) = server.registry();

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

    /// Model a session already riding the current leg without running a
    /// `connect` (the server may be deliberately silent): install it as Proven
    /// on the live leg through the supervisor's test seam.
    async fn ride_leg(registry: &SessionRegistry, session_id: &str, sink: Arc<dyn FrameSink>) {
        let (tx, rx) = oneshot::channel();
        registry
            .supervisor()
            .send(Msg::InjectProvenForTest {
                session_id: session_id.to_string(),
                sink,
                reply: tx,
            })
            .expect("supervisor is alive");
        rx.await
            .expect("supervisor replies")
            .expect("a live leg to ride");
    }

    /// Abort the live pump so it can neither pump nor report its own death —
    /// the corpse shape a panicked pump (or one aborted mid-poll) leaves
    /// behind, which only the OTHER discovery channels can find.
    async fn kill_pump_silently(registry: &SessionRegistry) {
        let (tx, rx) = oneshot::channel();
        registry
            .supervisor()
            .send(Msg::AbortPumpForTest { reply: tx })
            .expect("supervisor is alive");
        rx.await.expect("supervisor replies").expect("abort");
    }

    /// The happy path of the send gate: a send on the very leg that proved the
    /// subscription is admitted and reaches the wire. (The gate exists to refuse
    /// stale legs — this pins that it never over-refuses the live one.)
    #[tokio::test]
    async fn a_send_on_the_leg_that_proved_the_subscription_is_admitted() {
        let mut server = Server::start().await;
        let (registry, _dials) = server.registry();

        registry
            .connect("s1", Arc::new(RecordingSink::default()))
            .await
            .expect("connect");
        assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));

        registry
            .send(
                "s1".to_string(),
                "hi".to_string(),
                "m1".to_string(),
                Vec::new(),
            )
            .await
            .expect("send on the proven leg");

        match server.next_frame().await {
            Frame::Message(message) => assert_eq!(message.platform_msg_id, "m1"),
            other => panic!("expected the user message, got {other:?}"),
        }
    }

    /// The cold-start black hole of 2026-08-16: the old leg dies, a foreground
    /// `preconnect` installs a fresh leg that subscribes NOTHING, and a send for
    /// the still-sink-holding session used to return `Ok` while the gateway
    /// silently dropped it as not-subscribed. The send gate must refuse instead,
    /// so the client redials (subscribe + send) rather than spinning forever.
    #[tokio::test]
    async fn a_send_on_a_fresh_unsubscribed_leg_is_refused_not_black_holed() {
        let mut server = Server::start().await;
        let (registry, _dials) = server.registry();
        let sink = Arc::new(RecordingSink::default());

        registry.connect("s1", sink.clone()).await.expect("connect");
        assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));

        // The leg dies out from under the session…
        server.close(CloseFrame {
            code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Away,
            reason: "gone".into(),
        });
        let mut waited = Duration::ZERO;
        while sink.disconnects().is_empty() && waited < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(20)).await;
            waited += Duration::from_millis(20);
        }
        assert_eq!(sink.disconnects(), ["s1".to_string()]);

        // …and a foreground probe warms a replacement that carries no
        // subscriptions.
        registry.preconnect().await.expect("preconnect");
        assert!(
            server.stayed_quiet(Duration::from_millis(200)).await,
            "preconnect must not subscribe anything"
        );

        let err = registry
            .send(
                "s1".to_string(),
                "hi".to_string(),
                "m1".to_string(),
                Vec::new(),
            )
            .await
            .expect_err("a send on a leg that never saw this session's Subscribe");
        assert!(matches!(err, TransportError::NotConnected), "got {err:?}");
        assert!(
            server.stayed_quiet(Duration::from_millis(200)).await,
            "the refused send must never reach the wire"
        );
    }

    /// Death is handled exactly once however many channels report it: after
    /// the pump's own tail already announced the death, a probe walking over
    /// the same corpse must not re-deliver `on_disconnected` (the late report
    /// no-ops on the leg-id mismatch).
    #[tokio::test]
    async fn a_duplicate_death_report_is_not_redelivered() {
        let mut server = Server::start().await;
        let (registry, _dials) = server.registry();
        let sink = Arc::new(RecordingSink::default());

        registry.connect("s1", sink.clone()).await.expect("connect");
        assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));

        server.close(CloseFrame {
            code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Away,
            reason: "gone".into(),
        });
        let mut waited = Duration::ZERO;
        while sink.disconnects().is_empty() && waited < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(20)).await;
            waited += Duration::from_millis(20);
        }

        // The probe redials a fresh leg; the old death must not echo again.
        registry.preconnect().await.expect("preconnect");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            sink.disconnects(),
            ["s1".to_string()],
            "exactly one on_disconnected across every discovery channel"
        );
    }

    /// The corpse a pump leaves when it dies WITHOUT reporting (a panic, or an
    /// abort landing mid-poll): the enqueue-failure discovery channel must
    /// still deliver `on_disconnected` — a silently-swallowed death is the
    /// cold-start black hole's client half.
    #[tokio::test]
    async fn a_corpse_found_by_a_send_still_tells_the_riders() {
        let mut server = Server::start().await;
        let (registry, _dials) = server.registry();
        let sink = Arc::new(RecordingSink::default());

        registry.connect("s1", sink.clone()).await.expect("connect");
        assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));

        kill_pump_silently(&registry).await;

        let err = registry
            .send(
                "s1".to_string(),
                "hi".to_string(),
                "m1".to_string(),
                Vec::new(),
            )
            .await
            .expect_err("the enqueue must discover the corpse");
        assert!(matches!(err, TransportError::NotConnected), "got {err:?}");
        let mut waited = Duration::ZERO;
        while sink.disconnects().is_empty() && waited < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(20)).await;
            waited += Duration::from_millis(20);
        }
        assert_eq!(
            sink.disconnects(),
            ["s1".to_string()],
            "a death only a failed enqueue can see must still be announced"
        );
    }

    /// The death fan-out is targeted: a session mid-redial has already been
    /// withdrawn from the dying leg by its own open, so the death its open
    /// discovers is announced to the OTHER riders and never to the dial it is
    /// about to prove — a spurious report would make the client distrust a
    /// perfectly good reconnect.
    #[tokio::test]
    async fn a_death_found_by_a_reopen_spares_the_reopening_session() {
        let mut server = Server::start().await;
        let (registry, _dials) = server.registry();
        let first_sink = Arc::new(RecordingSink::default());
        let bystander = Arc::new(RecordingSink::default());

        registry
            .connect("s1", first_sink.clone())
            .await
            .expect("connect s1");
        assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));
        registry
            .connect("s2", bystander.clone())
            .await
            .expect("connect s2");
        assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));

        kill_pump_silently(&registry).await;

        // s1 reopens; its open discovers the corpse, which kicks s2's redial
        // but not s1's own fresh dial.
        let second_sink = Arc::new(RecordingSink::default());
        registry
            .connect("s1", second_sink.clone())
            .await
            .expect("reopen s1 over the corpse");
        assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));

        let mut waited = Duration::ZERO;
        while bystander.disconnects().is_empty() && waited < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(20)).await;
            waited += Duration::from_millis(20);
        }
        assert_eq!(bystander.disconnects(), ["s2".to_string()]);
        assert!(
            first_sink.disconnects().is_empty() && second_sink.disconnects().is_empty(),
            "the reopening session must not hear the death its own open discovered"
        );
    }

    /// The ack-failure withdrawal: a subscribe the gateway never acknowledged
    /// must leave NO leg binding behind, or the send gate would `Ok` sends
    /// onto a live leg whose gateway subscription does not exist — the silent
    /// server-side drop again.
    #[tokio::test]
    async fn a_send_after_an_unacknowledged_subscribe_is_refused() {
        let mut server = Server::silent().await;
        let (registry, _dials) = server.registry();
        let registry = registry.with_subscribe_ack_timeout(TEST_ACK_BUDGET_BEATABLE);

        // Keep the leg alive across the failed connect so the retire branch
        // doesn't fire — this is the "leg is live; not touching it" path.
        registry.preconnect().await.expect("preconnect");
        let connect = {
            let registry = &registry;
            async move {
                registry
                    .connect("s1", Arc::new(RecordingSink::default()))
                    .await
            }
        };
        let traffic = async {
            assert!(matches!(server.next_frame().await, Frame::Subscribe { .. }));
            server.send(Frame::Ping);
            assert!(matches!(server.next_frame().await, Frame::Pong));
            tokio::time::sleep(TEST_ACK_BUDGET_BEATABLE).await;
        };
        let (result, ()) = tokio::join!(connect, traffic);
        assert!(result.is_err(), "the subscribe was never acknowledged");

        let err = registry
            .send(
                "s1".to_string(),
                "hi".to_string(),
                "m1".to_string(),
                Vec::new(),
            )
            .await
            .expect_err("no proven subscription to send on");
        assert!(matches!(err, TransportError::NotConnected), "got {err:?}");
        assert!(
            server.stayed_quiet(Duration::from_millis(200)).await,
            "the refused send must never reach the wire"
        );
    }

    /// A dialer whose establish panics — the foreign dial stack has panicked
    /// before (a mis-featured rustls once panicked building its ClientConfig).
    struct PanickingDialer;

    impl LegDialer for PanickingDialer {
        fn establish(
            &self,
        ) -> futures_util::future::BoxFuture<'_, Result<Connection, TransportError>> {
            Box::pin(async { panic!("dial stack exploded") })
        }
    }

    /// `DialFinished` is the only exit from `Dialing` and its waiters' replies
    /// are held in the leg — so a dial child that dies without reporting must
    /// still produce the message (the send-on-drop report), or every parked
    /// and future connect hangs until app relaunch.
    #[tokio::test]
    async fn a_panicking_dial_child_fails_the_open_instead_of_hanging_it() {
        let registry = SessionRegistry::new(Arc::new(PanickingDialer));

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            registry.connect("s1", Arc::new(RecordingSink::default())),
        )
        .await
        .expect("the open must resolve, not hang");
        assert!(result.is_err(), "a panicked dial is a failed dial");

        // And the leg exited `Dialing`: the next operation gets its own
        // (failing) dial instead of parking forever behind the corpse.
        let again = tokio::time::timeout(Duration::from_secs(5), registry.preconnect())
            .await
            .expect("the follow-up must resolve, not hang");
        assert!(again.is_err());
    }

    /// A dialer that fails every attempt; the first attempt parks on a gate so
    /// a test can arrange latecomers deterministically.
    struct FailingDialer {
        dials: Arc<AtomicUsize>,
        first_gate: parking_lot::Mutex<Option<oneshot::Receiver<()>>>,
    }

    impl LegDialer for FailingDialer {
        fn establish(
            &self,
        ) -> futures_util::future::BoxFuture<'_, Result<Connection, TransportError>> {
            Box::pin(async move {
                self.dials.fetch_add(1, Ordering::Relaxed);
                let gate = self.first_gate.lock().take();
                if let Some(gate) = gate {
                    let _ = gate.await;
                }
                Err(TransportError::Other("dial refused".into()))
            })
        }
    }

    /// The per-batch retry contract: opens arriving DURING a failed dial share
    /// exactly ONE fresh attempt (adopters of the retry), and no more —
    /// recovery beyond that is the client's redial ladder.
    #[tokio::test]
    async fn latecomers_of_a_failed_dial_share_one_fresh_dial() {
        let (gate_tx, gate_rx) = oneshot::channel();
        let dials = Arc::new(AtomicUsize::new(0));
        let registry = Arc::new(SessionRegistry::new(Arc::new(FailingDialer {
            dials: dials.clone(),
            first_gate: parking_lot::Mutex::new(Some(gate_rx)),
        })));

        let adopter = tokio::spawn({
            let registry = registry.clone();
            async move {
                registry
                    .connect("s1", Arc::new(RecordingSink::default()))
                    .await
            }
        });
        let mut waited = Duration::ZERO;
        while dials.load(Ordering::Relaxed) == 0 && waited < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(5)).await;
            waited += Duration::from_millis(5);
        }
        let latecomer = tokio::spawn({
            let registry = registry.clone();
            async move {
                registry
                    .connect("s2", Arc::new(RecordingSink::default()))
                    .await
            }
        });
        // Let the latecomer's Open park on the in-flight dial before it fails.
        tokio::time::sleep(Duration::from_millis(50)).await;
        gate_tx
            .send(())
            .expect("the first dial is parked on the gate");

        let adopter = adopter.await.expect("adopter task");
        let latecomer = latecomer.await.expect("latecomer task");
        assert!(
            adopter.is_err(),
            "the adopter shares the first dial's failure"
        );
        assert!(
            latecomer.is_err(),
            "the latecomer shares the retry's failure"
        );
        assert_eq!(
            dials.load(Ordering::Relaxed),
            2,
            "the latecomer batch gets exactly one fresh dial"
        );
    }
}
