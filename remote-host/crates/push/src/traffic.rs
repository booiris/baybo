//! Per-`device_id` push traffic accounting — how many notifications each device was
//! sent and how many payload bytes egressed to APNs.
//!
//! Push is low-rate (one send per pushable turn, per-device rate-limited), so unlike
//! the relay's lock-free byte pump this just takes the map lock and increments under
//! it — no shared `Arc` handle is needed. A send is recorded **after** egress, for
//! every outcome (a send that APNs later rejects still consumed a request), keyed by
//! the device that was actually pushed. The record point sits past the
//! `store.get` / signature / rate checks, so the map is bounded by the *registered*
//! device set, never by forged ids.
//!
//! The server's flush task drains this the same way it drains the relay registry:
//! [`collect`](PushTrafficRegistry::collect) snapshots the deltas since the last
//! commit, the task persists them (UPSERT-accumulate), and on success
//! [`commit`](PushTrafficRegistry::commit) advances the baseline and evicts idle,
//! drained entries — re-creating an entry on a later send keeps accumulating into
//! the same durable row.

use std::collections::{HashMap, HashSet};

use parking_lot::Mutex;

/// Default ceiling on simultaneously-tracked devices, mirroring the device-token
/// store's soft cap so the two bound the same way. Past it, a brand-new device's
/// send is not recorded (rather than growing the map without bound).
pub const DEFAULT_PUSH_MAX_TRACKED: usize = crate::store::DEVICE_STORE_SOFT_CAP;

/// Default zero-traffic flush intervals before a drained device entry is evicted
/// (~5 min at the 60s flush cadence). A device that pushes less often than this just
/// re-creates its entry on the next send; the durable total is unaffected.
pub const DEFAULT_PUSH_IDLE_EVICT_EPOCHS: u32 = 5;

/// A device's send count + egress bytes: a per-interval delta from
/// [`collect`](PushTrafficRegistry::collect) or a cumulative total from
/// [`snapshot`](PushTrafficRegistry::snapshot).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PushCounts {
    /// APNs sends issued (every egress, regardless of APNs's verdict).
    pub sends: u64,
    /// Total payload bytes handed to APNs across those sends.
    pub bytes: u64,
}

impl PushCounts {
    fn delta_since(&self, base: &PushCounts) -> PushCounts {
        PushCounts {
            sends: self.sends.saturating_sub(base.sends),
            bytes: self.bytes.saturating_sub(base.bytes),
        }
    }

    fn is_zero(&self) -> bool {
        *self == PushCounts::default()
    }
}

/// One delta row from [`collect`](PushTrafficRegistry::collect).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushTrafficDelta {
    pub device_id: String,
    pub counts: PushCounts,
}

/// A device's running counts plus the flush task's private baseline / bookkeeping
/// (all under the map lock — no atomics needed at this rate).
#[derive(Default)]
struct Entry {
    live: PushCounts,
    flushed: PushCounts,
    pending: PushCounts,
    idle_epochs: u32,
}

/// Bound on the dropped-at-cap warn dedup set; past it the set is cleared (an
/// occasional repeat warn beats unbounded growth).
const REFUSED_WARN_DEDUP_CAP: usize = 1024;

/// Per-device push send/byte counters, bounded by [`DEFAULT_PUSH_MAX_TRACKED`] and
/// reclaimed by the flush task's idle eviction.
pub struct PushTrafficRegistry {
    map: Mutex<HashMap<String, Entry>>,
    max_tracked: usize,
    idle_evict_epochs: u32,
    /// Devices whose dropped-at-cap warn already fired, so a refused device warns
    /// once instead of once per send.
    refused_warned: Mutex<HashSet<String>>,
}

impl Default for PushTrafficRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PushTrafficRegistry {
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_PUSH_MAX_TRACKED, DEFAULT_PUSH_IDLE_EVICT_EPOCHS)
    }

    /// Construct with explicit bounds (tests exercise the cap + eviction cheaply).
    pub fn with_limits(max_tracked: usize, idle_evict_epochs: u32) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            max_tracked: max_tracked.max(1),
            idle_evict_epochs,
            refused_warned: Mutex::new(HashSet::new()),
        }
    }

    /// Record one APNs send of `bytes` for `device_id`. A new device past
    /// [`max_tracked`](Self::with_limits) is dropped (not recorded) rather than
    /// growing the map; an already-tracked device is always counted.
    pub fn record(&self, device_id: &str, bytes: usize) {
        let mut map = self.map.lock();
        let entry = if let Some(entry) = map.get_mut(device_id) {
            entry
        } else if map.len() < self.max_tracked {
            map.entry(device_id.to_string()).or_default()
        } else {
            let mut warned = self.refused_warned.lock();
            if warned.len() >= REFUSED_WARN_DEDUP_CAP {
                warned.clear();
            }
            if warned.insert(device_id.to_string()) {
                tracing::warn!(
                    device_id,
                    max_tracked = self.max_tracked,
                    "push: traffic ledger at cap — send not recorded for new device"
                );
            }
            return;
        };
        entry.live.sends = entry.live.sends.saturating_add(1);
        entry.live.bytes = entry.live.bytes.saturating_add(bytes as u64);
    }

    /// Snapshot each device once and return the non-zero deltas since the last
    /// [`commit`](Self::commit), stashing the snapshot as `pending`. Advances no
    /// baseline and evicts nothing, so a failed downstream write just re-collects.
    pub fn collect(&self) -> Vec<PushTrafficDelta> {
        let mut map = self.map.lock();
        let mut out = Vec::new();
        for (device_id, entry) in map.iter_mut() {
            let delta = entry.live.delta_since(&entry.flushed);
            entry.pending = entry.live;
            if !delta.is_zero() {
                out.push(PushTrafficDelta {
                    device_id: device_id.clone(),
                    counts: delta,
                });
            }
        }
        out
    }

    /// Promote the last [`collect`](Self::collect)'s `pending` to the persisted
    /// baseline (call only after its deltas were durably written), roll the idle
    /// bookkeeping, and evict drained entries idle past the threshold. A re-recorded
    /// device re-creates its entry; the UPSERT keeps accumulating into the same row.
    pub fn commit(&self) {
        let evict_epochs = self.idle_evict_epochs;
        let mut map = self.map.lock();
        map.retain(|_, entry| {
            let had_delta = entry.pending != entry.flushed;
            entry.flushed = entry.pending;
            entry.idle_epochs = if had_delta {
                0
            } else {
                entry.idle_epochs.saturating_add(1)
            };
            // Keep unless idle long enough AND fully drained (no record landed after
            // the last collect snapshot).
            entry.idle_epochs < evict_epochs || entry.live != entry.flushed
        });
    }

    /// Cumulative-since-start counts per tracked device (tests / future dashboard).
    pub fn snapshot(&self) -> Vec<PushTrafficDelta> {
        let map = self.map.lock();
        map.iter()
            .map(|(device_id, entry)| PushTrafficDelta {
                device_id: device_id.clone(),
                counts: entry.live,
            })
            .collect()
    }

    /// Number of devices currently tracked (test/observability helper).
    pub fn tracked_len(&self) -> usize {
        self.map.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_sums_sends_and_bytes_per_device() {
        let reg = PushTrafficRegistry::new();
        reg.record("dev-1", 100);
        reg.record("dev-1", 40);
        reg.record("dev-2", 7);

        let mut snap = reg.snapshot();
        snap.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        assert_eq!(snap.len(), 2);
        assert_eq!(
            snap[0].counts,
            PushCounts {
                sends: 2,
                bytes: 140
            }
        );
        assert_eq!(snap[1].counts, PushCounts { sends: 1, bytes: 7 });
    }

    #[test]
    fn collect_commit_collect_is_delta_since_last_commit() {
        let reg = PushTrafficRegistry::new();
        reg.record("d", 50);
        let first = reg.collect();
        assert_eq!(
            first[0].counts,
            PushCounts {
                sends: 1,
                bytes: 50
            }
        );
        reg.commit();
        assert!(reg.collect().is_empty(), "no fresh delta after commit");
        reg.record("d", 30);
        let second = reg.collect();
        assert_eq!(
            second[0].counts,
            PushCounts {
                sends: 1,
                bytes: 30
            }
        );
    }

    #[test]
    fn idle_drained_entries_are_evicted() {
        let reg = PushTrafficRegistry::with_limits(64, 1);
        reg.record("d", 10);
        // Interval 1: a delta resets idle; not evicted.
        reg.collect();
        reg.commit();
        assert_eq!(reg.tracked_len(), 1);
        // Interval 2: no new traffic, drained → evicted at the threshold.
        reg.collect();
        reg.commit();
        assert_eq!(reg.tracked_len(), 0, "idle drained device reclaimed");
        // A later send re-creates the entry and keeps recording.
        reg.record("d", 5);
        assert_eq!(reg.snapshot()[0].counts, PushCounts { sends: 1, bytes: 5 });
    }

    #[test]
    fn at_cap_a_new_device_is_not_recorded() {
        let reg = PushTrafficRegistry::with_limits(1, 5);
        reg.record("a", 5);
        reg.record("b", 99); // map full → dropped
        assert_eq!(reg.tracked_len(), 1);
        assert_eq!(reg.snapshot()[0].device_id, "a");
        // The tracked device keeps counting even at the cap.
        reg.record("a", 1);
        assert_eq!(reg.snapshot()[0].counts, PushCounts { sends: 2, bytes: 6 });
    }
}
