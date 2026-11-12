//! The blind byte-pipe broker — "match two legs by key, copy bytes blind."
//!
//! This is the single relay primitive the whole connectivity story rides on.
//! Both uses key into the same broker:
//!
//! - **pairing rendezvous** — keyed by the public `rendezvous_id` (the app and
//!   the gateway each open a leg; the broker copies opaque Noise frames),
//! - **content relay** — keyed by the C-assigned `relay_node_id` (the phone and
//!   the NAT'd gateway each open a leg; the broker copies Noise frames).
//!
//! The broker NEVER inspects the bytes: pairing rides Noise `XXpsk0` (C sees only
//! the public `rendezvous_id` and ciphertext — the QR secret that authenticates
//! the handshake never reaches C, so C cannot complete it) and content rides
//! Noise inside TLS (C sees only ciphertext). It only matches a key to a partner
//! and shuttles opaque frames.
//!
//! The matching + piping core lives here over `mpsc` channels so it is fully
//! host-testable; the production WebSocket transport is a thin adapter that
//! pumps each socket's binary frames into/out of a [`RelayLeg`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use remote_host_protocol::key_tag;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

/// Most frames one direction of a leg may have queued toward the peer. The
/// byte budget below is the bound that matters for memory; this one only stops
/// a stream of tiny frames from queueing without limit.
const LEG_CHANNEL_CAP: usize = 256;

/// Most bytes one direction of a leg may have queued toward the peer before its
/// sender waits. The queue only fills when the receiving socket drains slower
/// than the throttle admits (a stalled gateway, a slow downlink, a peer that
/// stops reading); then a frame-count cap alone would let one leg hold
/// 256 × 128 KiB = 32 MiB per direction. Waiting here stops the pump reading
/// its socket, so the backlog moves to the sender's TCP buffers. One MiB is a
/// second of the default per-key rate, so a healthy peer never meets it.
const LEG_BUFFER_BYTES: u32 = 1024 * 1024;

/// How long a parked leg waits for its partner before it's reaped. A WS upgrade
/// that fails *after* the synchronous park leaves a half uncancelled (its
/// `on_upgrade` cleanup never runs), and a signalled gateway might never dial
/// its data leg; the sweep keeps those from accumulating. Well above any real
/// rendezvous latency (both legs arrive within seconds).
const PENDING_TTL: Duration = Duration::from_secs(120);

/// Hard ceiling on simultaneously-parked legs across **all** keys — a global
/// backstop the TTL sweep can't provide (the sweep only bounds *age*, not
/// *count*, so a fast enough spray of distinct keys could still spike memory
/// between sweeps). Once the map is at the cap a new park is refused (the caller
/// returns `503`), while matching an already-parked leg always succeeds (it
/// shrinks the map). Generous: a healthy relay parks a handful per gateway and
/// both legs of every rendezvous arrive within seconds, so the steady state is
/// far below this — only a flood approaches it.
const MAX_PENDING_LEGS: usize = 1024;

/// One end of a matched relay pair. Frames written to [`to_peer`](Self::to_peer)
/// surface at the partner leg's [`from_peer`](Self::from_peer), blind.
pub struct RelayLeg {
    /// Opaque frames this leg sends toward its peer.
    pub to_peer: LegSender,
    /// Opaque frames the peer sent toward this leg.
    pub from_peer: LegReceiver,
}

/// The partner leg is gone, so a frame has nowhere to go.
#[derive(Debug)]
pub struct PeerGone;

/// A queued frame and the share of [`LEG_BUFFER_BYTES`] it holds until the
/// receiver takes it.
type QueuedFrame = (Vec<u8>, OwnedSemaphorePermit);

/// The sending half of one direction of a leg, bounded by
/// [`LEG_CHANNEL_CAP`] frames and [`LEG_BUFFER_BYTES`] bytes.
pub struct LegSender {
    tx: mpsc::Sender<QueuedFrame>,
    budget: Arc<Semaphore>,
}

impl LegSender {
    /// Queue `frame` for the peer, waiting while the queue is at either bound.
    pub async fn send(&self, frame: Vec<u8>) -> Result<(), PeerGone> {
        let cost = u32::try_from(frame.len())
            .unwrap_or(u32::MAX)
            .clamp(1, LEG_BUFFER_BYTES);
        let permit = Arc::clone(&self.budget)
            .acquire_many_owned(cost)
            .await
            .map_err(|_| PeerGone)?;
        self.tx.send((frame, permit)).await.map_err(|_| PeerGone)
    }
}

/// The receiving half of one direction of a leg. Dropping it drops whatever is
/// still queued, which hands the budget back and lets a waiting
/// [`LegSender::send`] fail with [`PeerGone`] instead of hanging.
pub struct LegReceiver {
    rx: mpsc::Receiver<QueuedFrame>,
}

impl LegReceiver {
    /// The next frame from the peer, or `None` once the peer is gone.
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        self.rx.recv().await.map(|(frame, _budget)| frame)
    }
}

fn leg_channel() -> (LegSender, LegReceiver) {
    let (tx, rx) = mpsc::channel(LEG_CHANNEL_CAP);
    let budget = Arc::new(Semaphore::new(LEG_BUFFER_BYTES as usize));
    (LegSender { tx, budget }, LegReceiver { rx })
}

/// Outcome of a park-only host [`join`](RelayBroker::park). A host never matches
/// an existing half, so the two non-parking outcomes are refusals the caller
/// turns into an HTTP status (the gateway then retries its host leg).
pub enum ParkOutcome {
    /// Parked; the leg waits for an app to [`try_match`](RelayBroker::try_match) it.
    Parked(RelayLeg),
    /// A live host already holds this rendezvous — refuse rather than splice two
    /// host legs together.
    Occupied,
    /// The global parked-leg ceiling ([`MAX_PENDING_LEGS`]) is reached.
    AtCapacity,
}

/// The first leg's counterpart channels, parked until a partner joins.
struct PendingHalf {
    /// First-leg → second-leg frames (the second leg reads these).
    from_first: LegReceiver,
    /// Second-leg → first-leg frames (the second leg writes these).
    to_first: LegSender,
    /// When this half was parked — used to reap it if no partner ever arrives.
    parked_at: Instant,
}

impl PendingHalf {
    /// A fresh pair: the first leg, and the half parked for its partner.
    fn open() -> (RelayLeg, Self) {
        let (first_to_second_tx, first_to_second_rx) = leg_channel();
        let (second_to_first_tx, second_to_first_rx) = leg_channel();
        let first = RelayLeg {
            to_peer: first_to_second_tx,
            from_peer: second_to_first_rx,
        };
        let half = Self {
            from_first: first_to_second_rx,
            to_first: second_to_first_tx,
            parked_at: Instant::now(),
        };
        (first, half)
    }

    /// The second leg, matched to this parked half.
    fn into_second(self) -> RelayLeg {
        RelayLeg {
            to_peer: self.to_first,
            from_peer: self.from_first,
        }
    }
}

/// Matches two legs by key and pipes opaque bytes between them, blind.
pub struct RelayBroker {
    pending: Mutex<HashMap<String, PendingHalf>>,
    /// Hard ceiling on simultaneously-parked legs (see [`MAX_PENDING_LEGS`]).
    max_pending: usize,
}

impl Default for RelayBroker {
    fn default() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            max_pending: MAX_PENDING_LEGS,
        }
    }
}

impl RelayBroker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the parked-leg ceiling (tests; an operator-tunable cap could ride
    /// this later). Clamped to ≥ 1 so the broker can always park at least one leg.
    pub fn with_max_pending(mut self, max: usize) -> Self {
        self.max_pending = max.max(1);
        self
    }

    /// Join the relay under `key` — **symmetric**: the first caller parks, the
    /// second caller with the same key is **matched** to it (the parked half is
    /// removed and the two are spliced), and frames flow blind between them. Use
    /// this where either leg may arrive first **and the key is unique to the one
    /// pair** — the content data leg: the phone `/content/join` signals then
    /// parks, the gateway `/content/host` dials on the signal, and the order is
    /// not guaranteed, but the `relay_key` is C-issued and used by exactly those
    /// two legs.
    ///
    /// The pairing **host** must NOT use this: its `rendezvous_id` is public and
    /// shared, so a second host's `join` would be spliced to the first host (not
    /// to an app). It uses [`park`](Self::park) (park-only) instead, with the app
    /// side on [`try_match`](Self::try_match).
    ///
    /// Returns `None` only when a *new* park would exceed [`MAX_PENDING_LEGS`]
    /// (after a sweep) — the global flood backstop. Matching an already-parked
    /// leg never hits the cap (it removes an entry), so an in-progress rendezvous
    /// always completes.
    pub fn join(&self, key: &str) -> Option<RelayLeg> {
        let mut pending = self.pending.lock();
        if let Some(half) = pending.remove(key) {
            Some(half.into_second())
        } else {
            // About to park a new half: first reap any whose partner never
            // arrived, so a dropped upgrade or a no-show gateway can't grow the
            // map without bound. (Matching above is never swept — only parking.)
            Self::sweep_pending(&mut pending, PENDING_TTL);
            // Even after the sweep, refuse to park past the hard ceiling — the
            // sweep bounds age, this bounds count against a fast distinct-key spray.
            if pending.len() >= self.max_pending {
                tracing::warn!(
                    cap = self.max_pending,
                    pending = pending.len(),
                    key_tag = %key_tag(key),
                    "relay: parked-leg ceiling reached; refusing new park"
                );
                return None;
            }
            let (first, half) = PendingHalf::open();
            pending.insert(key.to_string(), half);
            Some(first)
        }
    }

    /// Park a host leg under `key`, **never** matching an existing half — the
    /// park-only, asymmetric host side of the pairing rendezvous (the app side
    /// uses [`try_match`](Self::try_match), so a host only ever parks and waits).
    ///
    /// Refusing to match a pre-existing half is what makes it **structurally
    /// impossible to splice two host legs to each other** — a re-host race, a
    /// duplicate, or a bug yields [`ParkOutcome::Occupied`] (the caller retries)
    /// rather than a host↔host pipe. A stale (expired) half is swept first, so
    /// `Occupied` means a *live* host genuinely holds the rendezvous.
    pub fn park(&self, key: &str) -> ParkOutcome {
        let mut pending = self.pending.lock();
        // Reap expired halves first, so a timed-out prior host doesn't block the
        // re-host, and the cap below counts only live legs.
        Self::sweep_pending(&mut pending, PENDING_TTL);
        if pending.contains_key(key) {
            return ParkOutcome::Occupied;
        }
        if pending.len() >= self.max_pending {
            return ParkOutcome::AtCapacity;
        }
        let (first, half) = PendingHalf::open();
        pending.insert(key.to_string(), half);
        ParkOutcome::Parked(first)
    }

    /// Drop pending legs parked longer than `ttl` (their partner never arrived),
    /// returning how many were reaped. Runs opportunistically on each [`join`]
    /// that parks; also exposed directly for tests / an explicit sweep.
    ///
    /// [`join`]: Self::join
    pub fn sweep(&self, ttl: Duration) -> usize {
        let mut pending = self.pending.lock();
        Self::sweep_pending(&mut pending, ttl)
    }

    fn sweep_pending(pending: &mut HashMap<String, PendingHalf>, ttl: Duration) -> usize {
        let now = Instant::now();
        let mut swept: Vec<String> = Vec::new();
        pending.retain(|key, half| {
            let age = now.duration_since(half.parked_at);
            if age < ttl {
                return true;
            }
            swept.push(format!("{} ({}s)", key_tag(key), age.as_secs()));
            false
        });
        if !swept.is_empty() {
            tracing::info!(
                reaped = swept.len(),
                keys = ?swept,
                ttl_secs = ttl.as_secs(),
                "relay: swept expired parked legs (partner never arrived)"
            );
        }
        swept.len()
    }

    /// The parked-leg ceiling (so a refusal at [`ParkOutcome::AtCapacity`] can
    /// name the cap it hit).
    pub(crate) fn max_pending(&self) -> usize {
        self.max_pending
    }

    /// Match an already-parked leg for `key` **without** parking a new one.
    /// Returns the second leg if a host is waiting, else `None`.
    ///
    /// This is the asymmetric half of the pairing rendezvous: only an admitted
    /// gateway [`join`](Self::join)s (parks) a rendezvous; an unauthenticated app
    /// uses `try_match`, so it can pair with a waiting host but can never occupy a
    /// rendezvous or flood the broker with parked legs.
    pub fn try_match(&self, key: &str) -> Option<RelayLeg> {
        self.pending
            .lock()
            .remove(key)
            .map(PendingHalf::into_second)
    }

    /// Drop a still-pending (unmatched) leg — a disconnect before the partner
    /// arrives, or a TTL sweep. Returns whether a parked leg was removed.
    pub fn cancel(&self, key: &str) -> bool {
        self.pending.lock().remove(key).is_some()
    }

    /// Number of legs currently parked waiting for a partner.
    pub fn pending_len(&self) -> usize {
        self.pending.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn matched_legs_pipe_blind_both_ways() {
        let broker = RelayBroker::new();
        let mut app = broker.join("node-1").expect("first leg parks");
        assert_eq!(broker.pending_len(), 1, "first leg parks");
        let mut gw = broker.join("node-1").expect("second leg matches");
        assert_eq!(broker.pending_len(), 0, "second leg matched");

        app.to_peer.send(b"app->gw".to_vec()).await.unwrap();
        assert_eq!(gw.from_peer.recv().await.unwrap(), b"app->gw");

        gw.to_peer.send(b"gw->app".to_vec()).await.unwrap();
        assert_eq!(app.from_peer.recv().await.unwrap(), b"gw->app");
    }

    #[tokio::test]
    async fn distinct_keys_do_not_match() {
        let broker = RelayBroker::new();
        let _a = broker.join("k1").expect("k1 parks");
        let _b = broker.join("k2").expect("k2 parks");
        assert_eq!(broker.pending_len(), 2);
    }

    #[test]
    fn park_is_refused_at_the_ceiling_but_matching_still_succeeds() {
        let broker = RelayBroker::new().with_max_pending(2);
        let _a = broker.join("k1").expect("1st park under the cap");
        let _b = broker.join("k2").expect("2nd park under the cap");
        // A third *distinct* key would park a new leg → refused at the ceiling.
        assert!(
            broker.join("k3").is_none(),
            "parking past the ceiling is refused"
        );
        assert_eq!(broker.pending_len(), 2, "the refused park added nothing");
        // Matching an already-parked key shrinks the map, so it is never capped.
        let _match = broker
            .join("k1")
            .expect("matching an existing key is exempt");
        assert_eq!(broker.pending_len(), 1, "k1 matched and unparked");
        // The freed slot lets a new key park again.
        let _c = broker.join("k3").expect("a freed slot admits a new park");
        assert_eq!(broker.pending_len(), 2);
    }

    #[tokio::test]
    async fn park_refuses_a_second_host_then_app_try_matches_the_first() {
        let broker = RelayBroker::new();
        // First host parks.
        let host = match broker.park("rid") {
            ParkOutcome::Parked(leg) => leg,
            other => panic!("first host should park, got {}", park_name(&other)),
        };
        assert_eq!(broker.pending_len(), 1);
        // A second host under the same id is refused — NOT spliced to the first.
        assert!(
            matches!(broker.park("rid"), ParkOutcome::Occupied),
            "a second host must be refused, never matched"
        );
        assert_eq!(broker.pending_len(), 1, "the refusal parked nothing");
        // The app side still matches the first host via try_match.
        let mut app = broker
            .try_match("rid")
            .expect("app matches the parked host");
        host.to_peer.send(b"a->p".to_vec()).await.unwrap();
        assert_eq!(app.from_peer.recv().await.unwrap(), b"a->p");
    }

    #[test]
    fn park_reuses_a_slot_freed_by_the_ttl_sweep() {
        let broker = RelayBroker::new();
        let _stale = match broker.park("rid") {
            ParkOutcome::Parked(leg) => leg,
            other => panic!("parks, got {}", park_name(&other)),
        };
        // Force the parked half to look expired, then a fresh park sweeps it and
        // succeeds rather than reporting Occupied on a dead host.
        {
            let mut p = broker.pending.lock();
            if let Some(h) = p.get_mut("rid") {
                h.parked_at = Instant::now() - PENDING_TTL - Duration::from_secs(1);
            }
        }
        assert!(
            matches!(broker.park("rid"), ParkOutcome::Parked(_)),
            "an expired host is swept, so the slot re-parks"
        );
    }

    #[test]
    fn park_refuses_at_the_pending_ceiling() {
        let broker = RelayBroker::new().with_max_pending(1);
        assert!(matches!(broker.park("a"), ParkOutcome::Parked(_)));
        assert!(
            matches!(broker.park("b"), ParkOutcome::AtCapacity),
            "a new host past the ceiling is refused"
        );
    }

    fn park_name(o: &ParkOutcome) -> &'static str {
        match o {
            ParkOutcome::Parked(_) => "Parked",
            ParkOutcome::Occupied => "Occupied",
            ParkOutcome::AtCapacity => "AtCapacity",
        }
    }

    #[test]
    fn sweep_reaps_only_stale_pending_legs() {
        let broker = RelayBroker::new();
        let _a = broker.join("k1").expect("k1 parks");
        let _b = broker.join("k2").expect("k2 parks");
        assert_eq!(broker.pending_len(), 2);
        // Nothing is older than a long TTL — both survive.
        assert_eq!(broker.sweep(Duration::from_secs(3600)), 0);
        assert_eq!(broker.pending_len(), 2);
        // A zero TTL treats every parked leg as stale.
        assert_eq!(broker.sweep(Duration::ZERO), 2);
        assert_eq!(broker.pending_len(), 0);
        // Sweeping an empty map is a no-op.
        assert_eq!(broker.sweep(Duration::ZERO), 0);
    }

    #[tokio::test]
    async fn cancel_drops_pending_leg() {
        let broker = RelayBroker::new();
        let _a = broker.join("k").expect("k parks");
        assert!(broker.cancel("k"));
        assert_eq!(broker.pending_len(), 0);
        assert!(!broker.cancel("k"), "cancel of an unknown key is a no-op");
    }

    #[tokio::test]
    async fn peer_drop_closes_the_pipe() {
        let broker = RelayBroker::new();
        let app = broker.join("k").expect("first leg parks");
        let mut gw = broker.join("k").expect("second leg matches");
        drop(app); // app disconnects
        // The peer's stream ends cleanly rather than hanging.
        assert!(gw.from_peer.recv().await.is_none());
    }

    /// A peer that stops reading holds at most [`LEG_BUFFER_BYTES`] of the other
    /// side's frames, not [`LEG_CHANNEL_CAP`] of them: past the budget the
    /// sender waits, and each frame the peer takes lets one more through.
    #[tokio::test(start_paused = true)]
    async fn a_peer_that_stops_reading_holds_only_the_byte_budget() {
        let broker = RelayBroker::new();
        let phone = broker.join("k").expect("first leg parks");
        let mut gateway = broker.join("k").expect("second leg matches");
        let frame = vec![0u8; 64 * 1024];
        let fits = LEG_BUFFER_BYTES as usize / frame.len();
        assert!(fits < LEG_CHANNEL_CAP, "the byte budget binds first");

        for _ in 0..fits {
            phone.to_peer.send(frame.clone()).await.unwrap();
        }
        let wait = Duration::from_secs(5);
        assert!(
            tokio::time::timeout(wait, phone.to_peer.send(frame.clone()))
                .await
                .is_err(),
            "a full budget makes the sender wait"
        );

        assert_eq!(gateway.from_peer.recv().await.unwrap().len(), frame.len());
        tokio::time::timeout(wait, phone.to_peer.send(frame.clone()))
            .await
            .expect("a frame taken frees room for the next")
            .unwrap();
    }

    #[tokio::test]
    async fn a_sender_waiting_on_a_full_budget_fails_once_the_peer_goes() {
        let broker = RelayBroker::new();
        let phone = broker.join("k").expect("first leg parks");
        let gateway = broker.join("k").expect("second leg matches");
        let frame = vec![0u8; LEG_BUFFER_BYTES as usize];
        phone.to_peer.send(frame.clone()).await.unwrap();

        let blocked = tokio::spawn(async move { phone.to_peer.send(frame).await });
        tokio::task::yield_now().await;
        drop(gateway);
        assert!(
            blocked.await.unwrap().is_err(),
            "a vanished peer must not hang the sender"
        );
    }

    #[tokio::test]
    async fn try_match_pairs_with_host_but_never_parks() {
        let broker = RelayBroker::new();
        // No host yet → try_match parks nothing and returns None.
        assert!(broker.try_match("code-1").is_none());
        assert_eq!(broker.pending_len(), 0, "try_match never parks a leg");

        // The admitted gateway hosts the code (parks); the app then matches.
        let mut host = broker.join("code-1").expect("host parks");
        assert_eq!(broker.pending_len(), 1);
        let mut app = broker.try_match("code-1").expect("host was waiting");
        assert_eq!(broker.pending_len(), 0);

        host.to_peer.send(b"a->p".to_vec()).await.unwrap();
        assert_eq!(app.from_peer.recv().await.unwrap(), b"a->p");
        app.to_peer.send(b"p->a".to_vec()).await.unwrap();
        assert_eq!(host.from_peer.recv().await.unwrap(), b"p->a");
    }
}
