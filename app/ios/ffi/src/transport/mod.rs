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
//! File layout mirrors the concern boundary: this file holds the shared
//! wire primitives every leg surface uses (`WsStream`, [`TransportError`],
//! [`FrameCodec`], the readers) plus the seams and the [`SessionRegistry`]
//! facade; [`supervisor`] is the lifecycle actor; [`pump`] is the hot path.
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
use std::time::Duration;

use futures_util::StreamExt;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::api::{DeckSink, FrameSink, ProjectSink, SessionListSink};
use crate::core::{Frame, MobileError, WireApprovalDecision, WireAttachment};

mod pump;
mod supervisor;
#[cfg(test)]
mod tests;

use supervisor::{Msg, Reply};

/// The concrete client socket both legs dial.
pub(crate) type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

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

pub(crate) type SharedProjectSink = Arc<parking_lot::Mutex<Option<Arc<dyn ProjectSink>>>>;

/// The shared per-session frame-routing map: pump reads on every inbound
/// frame, supervisor writes on open/unsubscribe/disconnect. One of the two
/// deliberate shared surfaces (see the module doc).
type RoutingMap = Arc<Mutex<HashMap<String, Arc<dyn FrameSink>>>>;

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
    /// Connection-global board invalidations (`ProjectChanged`) land here.
    project_sink: SharedProjectSink,
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
            project_sink: Arc::new(parking_lot::Mutex::new(None)),
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
            supervisor::spawn(
                self.dialer.clone(),
                self.sinks.clone(),
                self.list_sink.clone(),
                self.deck_sink.clone(),
                self.project_sink.clone(),
                self.subscribe_ack_timeout,
            )
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

    pub(crate) fn set_project_sink(&self, sink: Option<Arc<dyn ProjectSink>>) {
        *self.project_sink.lock() = sink;
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

pub(crate) fn set_project_sink<L: SessionLeg>(leg: &L, sink: Option<Arc<dyn ProjectSink>>) {
    leg.registry().set_project_sink(sink);
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
