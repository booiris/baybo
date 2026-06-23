//! The blind byte-pipe broker — "match two legs by key, copy bytes blind."
//!
//! This is the single relay primitive the whole connectivity story rides on.
//! Both uses key into the same broker:
//!
//! - **pairing rendezvous** — keyed by the SPAKE2 pairing code (the app and the
//!   gateway each open a leg; the broker copies opaque PAKE blobs between them),
//! - **content relay** — keyed by the C-assigned `relay_node_id` (the phone and
//!   the NAT'd gateway each open a leg; the broker copies Noise frames).
//!
//! The broker NEVER inspects the bytes: pairing rides SPAKE2 (C learns neither
//! the code nor the derived key) and content rides Noise inside TLS (C sees only
//! ciphertext). It only matches a key to a partner and shuttles opaque frames.
//!
//! The matching + piping core lives here over `mpsc` channels so it is fully
//! host-testable; the production WebSocket transport is a thin adapter that
//! pumps each socket's binary frames into/out of a [`RelayLeg`].

use std::collections::HashMap;

use parking_lot::Mutex;
use tokio::sync::mpsc;

/// Per-leg channel capacity (opaque frames buffered toward the peer before
/// backpressure).
const LEG_CHANNEL_CAP: usize = 256;

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
}

/// Matches two legs by key and pipes opaque bytes between them, blind.
#[derive(Default)]
pub struct RelayBroker {
    pending: Mutex<HashMap<String, PendingHalf>>,
}

impl RelayBroker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Join the relay under `key`. The first caller gets a leg and parks; the
    /// second caller with the same key is matched, and frames then flow blind
    /// between the two. A re-join while a leg is still pending replaces the
    /// parked half (last-writer-wins on a stale leg).
    pub fn join(&self, key: &str) -> RelayLeg {
        let mut pending = self.pending.lock();
        if let Some(half) = pending.remove(key) {
            // Second leg: write toward the first, read what the first writes.
            RelayLeg {
                to_peer: half.to_first,
                from_peer: half.from_first,
            }
        } else {
            let (first_to_second_tx, first_to_second_rx) = mpsc::channel(LEG_CHANNEL_CAP);
            let (second_to_first_tx, second_to_first_rx) = mpsc::channel(LEG_CHANNEL_CAP);
            pending.insert(
                key.to_string(),
                PendingHalf {
                    from_first: first_to_second_rx,
                    to_first: second_to_first_tx,
                },
            );
            RelayLeg {
                to_peer: first_to_second_tx,
                from_peer: second_to_first_rx,
            }
        }
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
        let mut app = broker.join("node-1");
        assert_eq!(broker.pending_len(), 1, "first leg parks");
        let mut gw = broker.join("node-1");
        assert_eq!(broker.pending_len(), 0, "second leg matched");

        app.to_peer.send(b"app->gw".to_vec()).await.unwrap();
        assert_eq!(gw.from_peer.recv().await.unwrap(), b"app->gw");

        gw.to_peer.send(b"gw->app".to_vec()).await.unwrap();
        assert_eq!(app.from_peer.recv().await.unwrap(), b"gw->app");
    }

    #[tokio::test]
    async fn distinct_keys_do_not_match() {
        let broker = RelayBroker::new();
        let _a = broker.join("k1");
        let _b = broker.join("k2");
        assert_eq!(broker.pending_len(), 2);
    }

    #[tokio::test]
    async fn cancel_drops_pending_leg() {
        let broker = RelayBroker::new();
        let _a = broker.join("k");
        assert!(broker.cancel("k"));
        assert_eq!(broker.pending_len(), 0);
        assert!(!broker.cancel("k"), "cancel of an unknown key is a no-op");
    }

    #[tokio::test]
    async fn peer_drop_closes_the_pipe() {
        let broker = RelayBroker::new();
        let app = broker.join("k");
        let mut gw = broker.join("k");
        drop(app); // app disconnects
        // The peer's stream ends cleanly rather than hanging.
        assert!(gw.from_peer.recv().await.is_none());
    }
}
