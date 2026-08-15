//! Bounded inbound-message dedup keyed by
//! `(channel_type, bot_id, platform_msg_id)`.
//!
//! A sidecar that replays its long-poll buffer after a restart, or any
//! transport-level retry path, can hand the gateway the same upstream
//! event twice. Without dedup, every replay turns into a duplicate
//! agent turn — extra LLM cost, duplicate replies to the user. The
//! gateway sees only one inbound MPSC; this layer rejects duplicates
//! before the message reaches the router so the agent sees each event
//! exactly once.
//!
//! Sized as a FIFO ring rather than an LRU: dedup only needs to catch
//! duplicates within a short replay window, not maintain hot-set
//! semantics. FIFO is half the bookkeeping of LRU and adequate.
//! Sidecars that don't supply `platform_msg_id` skip dedup entirely.

use std::collections::{HashSet, VecDeque};

use baybo_model::ChannelType;
use parking_lot::Mutex;

/// Cap on the number of recently-seen `(channel_type, bot_id, msg_id)`
/// tuples retained. Tuned to absorb several seconds of bursty inbound
/// across many bots without false positives. Per-process, not per-bot
/// — channels with very chatty bots don't starve quieter peers because
/// dedup keys are platform-scoped to each bot anyway. Doubled from 4096
/// when the three per-listener instances merged into one process-wide
/// window, so a bursty multiplexed bot can't evict a subscribed
/// client's recent keys three times sooner than before.
const DEFAULT_CAPACITY: usize = 8192;

#[derive(Hash, Eq, PartialEq, Clone)]
struct Key {
    channel_type: ChannelType,
    bot_id: String,
    msg_id: String,
}

struct Inner {
    set: HashSet<Key>,
    fifo: VecDeque<Key>,
}

pub struct InboundDedup {
    inner: Mutex<Inner>,
    capacity: usize,
}

impl InboundDedup {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                set: HashSet::with_capacity(capacity),
                fifo: VecDeque::with_capacity(capacity),
            }),
            capacity,
        }
    }

    /// Record `(channel_type, bot_id, msg_id)` and return `true` when
    /// it was a fresh inbound, `false` when the gateway has already
    /// processed it within the dedup window. Empty `msg_id` always
    /// returns `true` — sidecars without a stable platform id opt out
    /// of dedup by leaving the field empty.
    pub fn check_and_record(&self, channel_type: &ChannelType, bot_id: &str, msg_id: &str) -> bool {
        if msg_id.is_empty() {
            return true;
        }
        let key = Key {
            channel_type: channel_type.clone(),
            bot_id: bot_id.to_owned(),
            msg_id: msg_id.to_owned(),
        };
        let mut inner = self.inner.lock();
        if !inner.set.insert(key.clone()) {
            return false;
        }
        inner.fifo.push_back(key);
        while inner.fifo.len() > self.capacity {
            if let Some(evict) = inner.fifo.pop_front() {
                inner.set.remove(&evict);
            }
        }
        true
    }

    /// Forget a recorded key so a retry can be admitted again. The router
    /// calls this when a message it REJECTED (rate limit, cost cap,
    /// sanitizer, route failure) will never persist: the record was made
    /// before the gates ran, so leaving it would black-hole every
    /// retransmission — and the client outbox retries under the same
    /// `platform_msg_id` by design, so the send would be permanently
    /// unsendable. Empty `msg_id` is a no-op, mirroring the
    /// `check_and_record` opt-out. Removing from the FIFO too is
    /// load-bearing: a set-only remove followed by a re-record would leave
    /// a stale FIFO twin whose later eviction silently deletes the LIVE
    /// set entry. `retain` is O(n), fine at gate-rejection rates.
    pub fn remove(&self, channel_type: &ChannelType, bot_id: &str, msg_id: &str) {
        if msg_id.is_empty() {
            return;
        }
        let key = Key {
            channel_type: channel_type.clone(),
            bot_id: bot_id.to_owned(),
            msg_id: msg_id.to_owned(),
        };
        let mut inner = self.inner.lock();
        if inner.set.remove(&key) {
            inner.fifo.retain(|k| k != &key);
        }
    }
}

impl Default for InboundDedup {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ct() -> ChannelType {
        ChannelType::from("weixin")
    }

    #[test]
    fn empty_msg_id_always_admitted() {
        let dedup = InboundDedup::new();
        assert!(dedup.check_and_record(&ct(), "bot-1", ""));
        assert!(dedup.check_and_record(&ct(), "bot-1", ""));
    }

    #[test]
    fn fresh_admitted_duplicate_rejected() {
        let dedup = InboundDedup::new();
        assert!(dedup.check_and_record(&ct(), "bot-1", "msg-A"));
        assert!(!dedup.check_and_record(&ct(), "bot-1", "msg-A"));
    }

    #[test]
    fn distinct_bots_never_collide() {
        let dedup = InboundDedup::new();
        assert!(dedup.check_and_record(&ct(), "bot-1", "msg-A"));
        assert!(dedup.check_and_record(&ct(), "bot-2", "msg-A"));
    }

    #[test]
    fn distinct_channels_never_collide() {
        let dedup = InboundDedup::new();
        let weixin = ChannelType::from("weixin");
        let telegram = ChannelType::from("telegram");
        assert!(dedup.check_and_record(&weixin, "bot-1", "1"));
        assert!(dedup.check_and_record(&telegram, "bot-1", "1"));
    }

    #[test]
    fn removed_key_is_admitted_again() {
        let dedup = InboundDedup::new();
        assert!(dedup.check_and_record(&ct(), "bot-1", "msg-A"));
        assert!(!dedup.check_and_record(&ct(), "bot-1", "msg-A"));
        dedup.remove(&ct(), "bot-1", "msg-A");
        assert!(dedup.check_and_record(&ct(), "bot-1", "msg-A"));
    }

    #[test]
    fn remove_is_scoped_and_empty_id_is_a_no_op() {
        let dedup = InboundDedup::new();
        assert!(dedup.check_and_record(&ct(), "bot-1", "msg-A"));
        dedup.remove(&ct(), "bot-2", "msg-A"); // different bot — not ours
        dedup.remove(&ct(), "bot-1", "msg-B"); // different id — not ours
        dedup.remove(&ct(), "bot-1", ""); // opt-out shape
        assert!(!dedup.check_and_record(&ct(), "bot-1", "msg-A"));
    }

    // The FIFO/set consistency `remove` must preserve: a set-only remove
    // would leave a stale FIFO twin, and once the key is re-recorded that
    // twin's eviction deletes the LIVE entry — re-admitting a duplicate.
    #[test]
    fn remove_then_rerecord_survives_eviction_pressure() {
        let dedup = InboundDedup::with_capacity(3);
        assert!(dedup.check_and_record(&ct(), "bot-1", "a"));
        dedup.remove(&ct(), "bot-1", "a");
        assert!(dedup.check_and_record(&ct(), "bot-1", "a"));
        // Two more fills the window; the stale-twin bug would have "a"'s
        // ghost evicted here, deleting the live entry.
        assert!(dedup.check_and_record(&ct(), "bot-1", "b"));
        assert!(dedup.check_and_record(&ct(), "bot-1", "c"));
        assert!(!dedup.check_and_record(&ct(), "bot-1", "a"));
    }

    #[test]
    fn fifo_evicts_oldest_past_capacity() {
        let dedup = InboundDedup::with_capacity(3);
        for id in ["a", "b", "c"] {
            assert!(dedup.check_and_record(&ct(), "bot-1", id));
        }
        // Window is {a, b, c} — all three are duplicates.
        assert!(!dedup.check_and_record(&ct(), "bot-1", "a"));
        assert!(!dedup.check_and_record(&ct(), "bot-1", "b"));
        assert!(!dedup.check_and_record(&ct(), "bot-1", "c"));
        // Inserting "d" evicts "a"; "a" can be re-admitted.
        assert!(dedup.check_and_record(&ct(), "bot-1", "d"));
        assert!(dedup.check_and_record(&ct(), "bot-1", "a"));
    }
}
