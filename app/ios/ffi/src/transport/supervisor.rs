//! The connection supervisor: one task per leg registry that owns every
//! lifecycle decision — dial coalescing, leg install, session subscription
//! phases, the send gate, the ack-timeout judgment, and the single
//! [`Supervisor::leg_death`] transition. See the module doc in
//! [`super`](crate::transport) for the invariants this buys, and
//! `app/ios/docs/connection.md` for each one's scar.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

use crate::api::FrameSink;
use crate::core::{WireApprovalDecision, WireAttachment};

use super::pump::{LegActivity, PumpCtx, pump};
use super::{
    Connection, LegDialer, OutboundMessage, RotationGate, RoutingMap, SharedDeckSink,
    SharedListSink, SharedProjectSink, TransportError,
};

/// Upper bound on a whole [`SessionRegistry::connect`] dial + handshake. Without
/// it a server that upgrades then never completes the handshake
/// would wedge `connect` with the `connecting` flag held, deadlocking every later
/// reconnect.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// A chat rotation waits for this long with no `Send`, no `ResolveApproval`
/// and no inbound frame but keepalives: a lull, not just a turn boundary.
pub(super) const CHAT_ROTATION_QUIET: Duration = Duration::from_secs(5);

/// The netns matrix holds rotation off so an idle phase crosses the NATs
/// with nothing but the carrier's QUIC keepalives.
#[cfg(feature = "test-support")]
pub(crate) static ROTATION_HELD: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// A request handed to the pump task to build + seal + send on the live leg.
pub(super) enum OutboundCmd {
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

/// A supervisor request's reply channel. Dropped receivers (a cancelled FFI
/// call) make the send a no-op.
pub(super) type Reply = oneshot::Sender<Result<(), TransportError>>;

/// Everything that reaches the supervisor loop: FFI operations (each carrying
/// its reply) and internal lifecycle events from the pump, the dial child, and
/// the ack timers.
pub(super) enum Msg {
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
    /// The dialer's rotation target may have appeared or gone.
    RotationTargetChanged,
    /// Re-judge the rotation's idle predicate.
    RotationCheck,
    /// A turn started on leg `leg_id`: an uncommitted rotation is abandoned.
    TurnStarted {
        leg_id: u64,
    },
    /// The rotation dial on leg `leg_id` finished.
    RotationFinished {
        leg_id: u64,
        result: Result<Box<Connection>, TransportError>,
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
        /// The pump's active-turn set and last-frame stamp, which the
        /// rotation's idle predicate reads.
        activity: Arc<parking_lot::Mutex<LegActivity>>,
        /// The leg rides a direct carrier, so it never rotates.
        on_carrier: bool,
        rotation: Option<Rotation>,
    },
}

/// A chat rotation in flight on the live leg: a chat leg dialing on the
/// direct carrier, to replace the relay leg once it is up.
struct Rotation {
    gate: RotationGate,
    task: tokio::task::JoinHandle<()>,
    /// `Send` / `ResolveApproval` (and, past the commit point, `Open`)
    /// held until the rotation resolves, then replayed onto whichever leg is
    /// current.
    held: Vec<Msg>,
    /// The relay leg died while the rotation was past its commit point; its
    /// death waits for the rotation's outcome.
    relay_ended: bool,
    started: Instant,
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
    project_sink: SharedProjectSink,
    ack_budget: Duration,
    rotation_quiet: Duration,
    /// The dialer has a rotation target to watch.
    rotates: bool,
    /// When the last `Send` or `ResolveApproval` went out.
    last_command: Instant,
    rotation_check: Option<AbortHandle>,
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
        let Some(msg) = self.hold_for_rotation(msg) else {
            return;
        };
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
            Msg::RotationTargetChanged => {
                self.abandon_rotation("target_changed");
                self.arm_rotation_check(Duration::ZERO);
            }
            Msg::RotationCheck => self.rotation_check(),
            Msg::TurnStarted { leg_id } => {
                if self.live_leg_id() == Some(leg_id) {
                    self.abandon_rotation("turn_started");
                }
            }
            Msg::RotationFinished { leg_id, result } => self.rotation_finished(leg_id, result),
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
        self.park(session_id, leg_id, vec![reply]);
    }

    /// [`Self::park_subscribing`] for any number of waiters, none included: a
    /// rotation re-subscribes sessions nobody is waiting on.
    fn park(&mut self, session_id: String, leg_id: u64, waiters: Vec<Reply>) {
        self.next_id += 1;
        let attempt = self.next_id;
        let parked_at = Instant::now();
        let Some(session) = self.sessions.get_mut(&session_id) else {
            // `open` inserted the entry moments ago; its absence means an
            // interleaved teardown drained the map — fail rather than hang.
            for waiter in waiters {
                let _ = waiter.send(Err(TransportError::SessionClosed));
            }
            return;
        };
        session.phase = Phase::Subscribing {
            on_leg: leg_id,
            attempt,
            waiters,
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
        self.last_command = Instant::now();
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
        self.last_command = Instant::now();
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
                        let mut socket = conn.socket;
                        socket.close().await;
                    });
                    log::info!("dial discarded: superseded by a teardown mid-dial");
                }
                return;
            }
        };
        match result {
            Ok(conn) => {
                let on_carrier = conn.socket.confirms_handshake();
                let outbound_tx = self.install_pump(leg_id, *conn, on_carrier);
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

    /// `on_carrier`: the leg rides the rotation target already, so it never
    /// rotates.
    fn install_pump(
        &mut self,
        leg_id: u64,
        conn: Connection,
        on_carrier: bool,
    ) -> mpsc::UnboundedSender<OutboundCmd> {
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        // Seeded now, not at the pump's first poll: the handshake reply that
        // produced `conn` already crossed the wire, so the leg is proven as of
        // this instant.
        let last_inbound = Arc::new(parking_lot::Mutex::new(Instant::now()));
        let activity = Arc::new(parking_lot::Mutex::new(LegActivity::new()));
        let task = tokio::spawn(pump(
            conn,
            PumpCtx {
                sinks: self.sinks.clone(),
                list_sink: self.list_sink.clone(),
                deck_sink: self.deck_sink.clone(),
                project_sink: self.project_sink.clone(),
                last_inbound: last_inbound.clone(),
                activity: activity.clone(),
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
            activity,
            on_carrier,
            rotation: None,
        };
        if !on_carrier {
            self.arm_rotation_check(self.rotation_quiet);
        }
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
        // A rotation past its commit point owns what happens to this leg: on
        // success it is retired silently, on failure this death runs then.
        if let Leg::Live {
            rotation: Some(rotation),
            ..
        } = &mut self.leg
            && !rotation.gate.abandon()
        {
            rotation.relay_ended = true;
            return;
        }
        self.abandon_rotation("leg_died");
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
        if let Some(check) = self.rotation_check.take() {
            check.abort();
        }
        match std::mem::replace(&mut self.leg, Leg::Idle) {
            // Deliberate teardown: abort WITHOUT the death transition, so no
            // on_disconnected fires (the logout contract). A late PumpEnded
            // from this leg no-ops on the leg_id mismatch.
            Leg::Live { task, rotation, .. } => {
                task.abort();
                if let Some(rotation) = rotation {
                    rotation.task.abort();
                    for held in rotation.held {
                        fail_held(held);
                    }
                }
            }
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

    fn live_leg_id(&self) -> Option<u64> {
        match &self.leg {
            Leg::Live { leg_id, .. } => Some(*leg_id),
            Leg::Idle | Leg::Dialing { .. } => None,
        }
    }

    /// While a rotation is in flight, `Send` and `ResolveApproval` wait for
    /// its outcome. An `Open` abandons a rotation still before its commit
    /// point — a session about to subscribe is not idle — and past it waits
    /// too. Returns the message when it runs now.
    fn hold_for_rotation(&mut self, msg: Msg) -> Option<Msg> {
        if !matches!(
            &self.leg,
            Leg::Live {
                rotation: Some(_),
                ..
            }
        ) {
            return Some(msg);
        }
        let held = match msg {
            Msg::Send { .. } | Msg::ResolveApproval { .. } => msg,
            Msg::Open { .. } if self.abandon_rotation("open") => return Some(msg),
            Msg::Open { .. } => msg,
            other => return Some(other),
        };
        match &mut self.leg {
            Leg::Live {
                rotation: Some(rotation),
                ..
            } => {
                rotation.held.push(held);
                None
            }
            _ => Some(held),
        }
    }

    /// Abandon the live leg's rotation if it has not reached its commit
    /// point, replaying what it held. `false` when there is none, or it is
    /// past its commit point.
    fn abandon_rotation(&mut self, reason: &str) -> bool {
        let Leg::Live { rotation, .. } = &mut self.leg else {
            return false;
        };
        if !rotation
            .as_ref()
            .is_some_and(|rotation| rotation.gate.abandon())
        {
            return false;
        }
        let Some(abandoned) = rotation.take() else {
            return false;
        };
        abandoned.task.abort();
        log::info!(
            "chat_rotation outcome=abandoned reason={reason} waited_ms={}",
            abandoned.started.elapsed().as_millis()
        );
        self.replay(abandoned.held);
        // The predicate is judged again once the leg is idle again.
        self.arm_rotation_check(self.rotation_quiet);
        true
    }

    /// Re-queue held messages behind everything already queued, in order.
    fn replay(&self, held: Vec<Msg>) {
        for msg in held {
            let _ = self.tx.send(msg);
        }
    }

    fn arm_rotation_check(&mut self, delay: Duration) {
        if !self.rotates {
            return;
        }
        if let Some(previous) = self.rotation_check.take() {
            previous.abort();
        }
        let tx = self.tx.clone();
        let check = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(Msg::RotationCheck);
        });
        self.rotation_check = Some(check.abort_handle());
    }

    /// The rotation's idle predicate. A chat leg moves onto the direct
    /// carrier only when it is live on the relay with no rotation in flight,
    /// no session is subscribing, no turn is active, and nothing but
    /// keepalives crossed it for `rotation_quiet`: answer deltas are never
    /// replayed, so a switch mid-turn would leave a hole in the answer.
    fn rotation_check(&mut self) {
        self.rotation_check = None;
        let Leg::Live {
            leg_id,
            activity,
            on_carrier: false,
            rotation: None,
            ..
        } = &self.leg
        else {
            return;
        };
        let leg_id = *leg_id;
        if !self.dialer.can_rotate() {
            // Nothing to rotate onto; the next target change re-arms.
            return;
        }
        #[cfg(feature = "test-support")]
        if ROTATION_HELD.load(std::sync::atomic::Ordering::SeqCst) {
            self.arm_rotation_check(self.rotation_quiet);
            return;
        }
        let (turns, last_frame) = {
            let activity = activity.lock();
            (!activity.turns.is_empty(), activity.last_frame)
        };
        let subscribing = self
            .sessions
            .values()
            .any(|session| matches!(session.phase, Phase::Subscribing { .. }));
        if turns || subscribing {
            self.arm_rotation_check(self.rotation_quiet);
            return;
        }
        let quiet_for = last_frame.max(self.last_command).elapsed();
        if quiet_for < self.rotation_quiet {
            self.arm_rotation_check(self.rotation_quiet - quiet_for);
            return;
        }
        self.start_rotation(leg_id);
    }

    fn start_rotation(&mut self, leg_id: u64) {
        let gate = RotationGate::default();
        let dialer = self.dialer.clone();
        let report = RotationReport {
            tx: self.tx.clone(),
            leg_id,
            armed: true,
        };
        let dial_gate = gate.clone();
        let task = tokio::spawn(async move {
            let result = dialer.rotate(dial_gate).await.map(Box::new);
            report.finish(result);
        });
        if let Leg::Live { rotation, .. } = &mut self.leg {
            *rotation = Some(Rotation {
                gate,
                task,
                held: Vec::new(),
                relay_ended: false,
                started: Instant::now(),
            });
        }
    }

    fn rotation_finished(&mut self, leg_id: u64, result: Result<Box<Connection>, TransportError>) {
        let rotation = match &mut self.leg {
            Leg::Live {
                leg_id: current,
                rotation,
                ..
            } if *current == leg_id => rotation.take(),
            _ => None,
        };
        let Some(rotation) = rotation else {
            // Abandoned or superseded: the leg must not survive as an orphan.
            if let Ok(conn) = result {
                tokio::spawn(async move {
                    let mut socket = conn.socket;
                    socket.close().await;
                });
            }
            return;
        };
        let waited_ms = rotation.started.elapsed().as_millis();
        match result {
            Ok(conn) => {
                log::info!("chat_rotation outcome=ok waited_ms={waited_ms}");
                self.rotated(leg_id, *conn);
            }
            Err(e) => {
                log::info!("chat_rotation outcome=failed waited_ms={waited_ms} reason=\"{e}\"");
                if rotation.relay_ended {
                    self.leg_death(leg_id);
                } else {
                    // The dialer has judged the carrier (retired, or cooling
                    // off), so the next check will not simply retry it.
                    self.arm_rotation_check(self.rotation_quiet);
                }
            }
        }
        self.replay(rotation.held);
    }

    /// Install the rotated chat leg in place of the relay leg. The relay
    /// pump is retired without fan-out, the way `disconnect` retires one, and
    /// every session that rode it re-subscribes on the new leg.
    fn rotated(&mut self, relay_leg: u64, conn: Connection) {
        if let Leg::Live { task, .. } = std::mem::replace(&mut self.leg, Leg::Idle) {
            task.abort();
        }
        self.next_id += 1;
        let leg_id = self.next_id;
        let outbound_tx = self.install_pump(leg_id, conn, true);
        let riding: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.phase.rides() == Some(relay_leg))
            .map(|(session_id, _)| session_id.clone())
            .collect();
        for session_id in &riding {
            let waiters = match self
                .sessions
                .get_mut(session_id)
                .map(|session| std::mem::replace(&mut session.phase, Phase::Registered))
            {
                Some(Phase::Subscribing { waiters, .. }) => waiters,
                _ => Vec::new(),
            };
            self.park(session_id.clone(), leg_id, waiters);
        }
        let enqueued = riding.into_iter().all(|session_id| {
            outbound_tx
                .send(OutboundCmd::Subscribe { session_id })
                .is_ok()
        });
        if !enqueued {
            self.leg_death(leg_id);
        }
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

/// Fail a message held for a rotation that will never resolve (a teardown).
fn fail_held(msg: Msg) {
    let reply = match msg {
        Msg::Open { reply, .. } | Msg::Send { reply, .. } | Msg::ResolveApproval { reply, .. } => {
            reply
        }
        _ => return,
    };
    let _ = reply.send(Err(TransportError::SessionClosed));
}

/// Send-on-drop for the rotation dial, like `start_dial`'s report: held
/// messages wait on `RotationFinished`, so a rotation that dies without
/// reporting must still resolve.
struct RotationReport {
    tx: mpsc::UnboundedSender<Msg>,
    leg_id: u64,
    armed: bool,
}

impl RotationReport {
    fn finish(mut self, result: Result<Box<Connection>, TransportError>) {
        self.armed = false;
        let _ = self.tx.send(Msg::RotationFinished {
            leg_id: self.leg_id,
            result,
        });
    }
}

impl Drop for RotationReport {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.tx.send(Msg::RotationFinished {
                leg_id: self.leg_id,
                result: Err(TransportError::Other(
                    "rotation task died before reporting".into(),
                )),
            });
        }
    }
}

/// Spawn the supervisor loop for one registry, returning its queue. The loop
/// runs for the life of the process (the registry holds a sender forever).
pub(super) fn spawn(
    dialer: Arc<dyn LegDialer>,
    sinks: RoutingMap,
    list_sink: SharedListSink,
    deck_sink: SharedDeckSink,
    project_sink: SharedProjectSink,
    ack_budget: Duration,
    rotation_quiet: Duration,
) -> mpsc::UnboundedSender<Msg> {
    let (tx, rx) = mpsc::unbounded_channel();
    let rotation_events = dialer.rotation_events();
    if let Some(mut events) = rotation_events.clone() {
        let tx = tx.clone();
        tokio::spawn(async move {
            while events.changed().await.is_ok() {
                if tx.send(Msg::RotationTargetChanged).is_err() {
                    return;
                }
            }
        });
    }
    let supervisor = Supervisor {
        dialer,
        sinks,
        list_sink,
        deck_sink,
        project_sink,
        ack_budget,
        rotation_quiet,
        rotates: rotation_events.is_some(),
        last_command: Instant::now(),
        rotation_check: None,
        tx: tx.clone(),
        leg: Leg::Idle,
        sessions: HashMap::new(),
        next_id: 0,
    };
    tokio::spawn(supervisor.run(rx));
    tx
}
