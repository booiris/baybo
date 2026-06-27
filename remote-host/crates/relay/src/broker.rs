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
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::mpsc;

/// Per-leg channel capacity (opaque frames buffered toward the peer before
/// backpressure).
const LEG_CHANNEL_CAP: usize = 256;

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
    pub to_peer: mpsc::Sender<Vec<u8>>,
    /// Opaque frames the peer sent toward this leg.
    pub from_peer: mpsc::Receiver<Vec<u8>>,
}

/// The first leg's counterpart channels, parked until a partner joins.
struct PendingHalf {
    /// First-leg → second-leg frames (the second leg reads these).
    from_first: mpsc::Receiver<Vec<u8>>,
    /// Second-leg → first-leg frames (the second leg writes these).
    to_first: mpsc::Sender<Vec<u8>>,
    /// When this half was parked — used to reap it if no partner ever arrives.
    parked_at: Instant,
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

    /// Join the relay under `key`. The first caller gets a leg and parks; the
    /// second caller with the same key is matched, and frames then flow blind
    /// between the two. A re-join while a leg is still pending replaces the
    /// parked half (last-writer-wins on a stale leg).
    ///
    /// Returns `None` only when a *new* park would exceed [`MAX_PENDING_LEGS`]
    /// (after a sweep) — the global flood backstop. Matching an already-parked
    /// leg never hits the cap (it removes an entry), so an in-progress rendezvous
    /// always completes.
    pub fn join(&self, key: &str) -> Option<RelayLeg> {
        let mut pending = self.pending.lock();
        if let Some(half) = pending.remove(key) {
            // Second leg: write toward the first, read what the first writes.
            Some(RelayLeg {
                to_peer: half.to_first,
                from_peer: half.from_first,
            })
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
                    "relay: parked-leg ceiling reached; refusing new park"
                );
                return None;
            }
            let (first_to_second_tx, first_to_second_rx) = mpsc::channel(LEG_CHANNEL_CAP);
            let (second_to_first_tx, second_to_first_rx) = mpsc::channel(LEG_CHANNEL_CAP);
            pending.insert(
                key.to_string(),
                PendingHalf {
                    from_first: first_to_second_rx,
                    to_first: second_to_first_tx,
                    parked_at: Instant::now(),
                },
            );
            Some(RelayLeg {
                to_peer: first_to_second_tx,
                from_peer: second_to_first_rx,
            })
        }
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
        let before = pending.len();
        pending.retain(|_, half| now.duration_since(half.parked_at) < ttl);
        before - pending.len()
    }

    /// Match an already-parked leg for `key` **without** parking a new one.
    /// Returns the second leg if a host is waiting, else `None`.
    ///
    /// This is the asymmetric half of the pairing rendezvous: only an admitted
    /// gateway [`join`](Self::join)s (parks) a rendezvous; an unauthenticated app
    /// uses `try_match`, so it can pair with a waiting host but can never occupy a
    /// rendezvous or flood the broker with parked legs.
    pub fn try_match(&self, key: &str) -> Option<RelayLeg> {
        self.pending.lock().remove(key).map(|half| RelayLeg {
            to_peer: half.to_first,
            from_peer: half.from_first,
        })
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
