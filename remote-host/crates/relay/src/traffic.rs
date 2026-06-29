//! Lock-free relay traffic accounting — the durable side of "how much did each
//! tenant move", distinct from the [`BandwidthRegistry`](crate::bandwidth) which
//! only *throttles*.
//!
//! Content legs meter at two levels, but both resolve the **same**
//! `(remote_api_key, server_id)` (the gateway's admitted key and the
//! `relay_node_id` it named), so per-tenant traffic keys there. Each leg holds a
//! cheap [`TrafficMeter`] (an `Arc` to a counter set, cloned once at leg setup like
//! [`limiter_for`](crate::bandwidth::BandwidthRegistry::limiter_for)) and, on the
//! forward hot path, does nothing but two `fetch_add(Relaxed)` — no map lock, no
//! allocation. The phone leg meters [`Up`](Direction::Up) (phone→gateway) and the
//! gateway leg [`Down`](Direction::Down); both draw on one shared [`Counters`], so a
//! session's up + down accumulate on a single entry.
//!
//! A single off-hot-path flush task drains the registry on an interval:
//! [`collect`](TrafficRegistry::collect) snapshots each atomic **once** and returns
//! the deltas since the last commit; the task persists them (UPSERT-accumulate into
//! SQLite), and on success calls [`commit`](TrafficRegistry::commit) to advance the
//! baseline and evict dead entries. Snapshot-then-commit (rather than advancing the
//! baseline at read time) means a transient write failure simply retries the same
//! bytes next interval — no double-count, no loss.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering, fence};

use parking_lot::Mutex;

use crate::bandwidth::BandwidthLimiter;

/// Starting ceiling on simultaneously-tracked `(remote_api_key, server_id)` entries,
/// before the runtime sizes it to the live admission capacity via
/// [`set_max_tracked`](TrafficRegistry::set_max_tracked). Past the cap a brand-new
/// pair gets an inert meter rather than growing the map without bound. The server
/// recomputes the real cap as `2 × Σ(admitted keys' max_conns)` each flush (every
/// `(key, server)` entry needs a content leg, and an entry lingers ~5 min past the
/// leg's close — hence ×2), so it tracks admission edits instead of a magic number;
/// this const is just the interim value until that first recompute and the default
/// in tests.
pub const DEFAULT_MAX_TRACKED: usize = 8192;

/// Default number of consecutive zero-traffic flush intervals after which a fully
/// drained entry **with no live leg** is evicted. At the 60s default flush cadence
/// this keeps an idle `(key, server)` around for ~5 min before reclaiming its slot,
/// well past a normal lull between messages — and an *open* leg is never evicted
/// regardless (it still holds the meter `Arc`).
pub const DEFAULT_IDLE_EVICT_EPOCHS: u32 = 5;

/// Which direction a leg's bytes meter as. Both directions of a session draw on the
/// same `(key, server)` [`Counters`]; the direction only selects which pair of
/// fields a frame increments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Phone → gateway: the content-join leg's inbound (uplink).
    Up,
    /// Gateway → phone: the content-host leg's inbound (downlink).
    Down,
}

/// One `(remote_api_key, server_id)`'s live, monotonically-increasing counters,
/// shared via `Arc` with every leg metering against it. Only ever incremented on the
/// hot path (`Relaxed`, since a single flusher reads them) and read whole by the
/// flush task.
#[derive(Default, Debug)]
struct Counters {
    bytes_up: AtomicU64,
    bytes_down: AtomicU64,
    frames_up: AtomicU64,
    frames_down: AtomicU64,
}

impl Counters {
    /// Read every counter **once** into a plain snapshot. Called by the flush task
    /// to compute a delta; loading each atomic a single time is what keeps a
    /// concurrent hot-path `record` from being silently folded into the baseline
    /// without ever being emitted.
    fn load(&self) -> Counts {
        Counts {
            bytes_up: self.bytes_up.load(Ordering::Relaxed),
            bytes_down: self.bytes_down.load(Ordering::Relaxed),
            frames_up: self.frames_up.load(Ordering::Relaxed),
            frames_down: self.frames_down.load(Ordering::Relaxed),
        }
    }
}

/// A plain set of traffic counts: a per-interval **delta** when returned by
/// [`collect`](TrafficRegistry::collect), or a cumulative-since-start total when
/// returned by [`snapshot`](TrafficRegistry::snapshot). The SQLite sink adds a delta
/// onto the stored lifetime total.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub frames_up: u64,
    pub frames_down: u64,
}

impl Counts {
    /// `self − base`, field-wise (saturating; the counters are monotonic so this
    /// never actually underflows, but saturating keeps it total).
    fn delta_since(&self, base: &Counts) -> Counts {
        Counts {
            bytes_up: self.bytes_up.saturating_sub(base.bytes_up),
            bytes_down: self.bytes_down.saturating_sub(base.bytes_down),
            frames_up: self.frames_up.saturating_sub(base.frames_up),
            frames_down: self.frames_down.saturating_sub(base.frames_down),
        }
    }

    fn is_zero(&self) -> bool {
        *self == Counts::default()
    }
}

/// One delta row produced by [`collect`](TrafficRegistry::collect): a tenant/server
/// pair plus the bytes/frames it moved since the previous collect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayTrafficDelta {
    pub remote_api_key: String,
    pub server_id: String,
    pub counts: Counts,
}

/// A live tracked entry. `counters` is shared with the legs; `flushed` / `pending` /
/// `idle_epochs` are the flush task's private bookkeeping, only ever touched under
/// the map lock (never by a leg), so they need no atomics.
struct Entry {
    counters: Arc<Counters>,
    /// Counts already persisted (the baseline the next delta subtracts from).
    flushed: Counts,
    /// The snapshot captured by the last [`collect`], promoted to `flushed` by
    /// [`commit`] once its delta is durably written.
    pending: Counts,
    /// Consecutive flush intervals with no new traffic (reset on any delta).
    idle_epochs: u32,
}

/// Per-`(remote_api_key, server_id)` traffic counters. A leg's first metered frame
/// lazily creates its entry; subsequent legs for the same pair share it. Bounded by
/// the runtime cap ([`set_max_tracked`](Self::set_max_tracked), sized to the live
/// admission capacity; [`DEFAULT_MAX_TRACKED`] until first set) and reclaimed by the
/// flush task's idle eviction.
pub struct TrafficRegistry {
    map: Mutex<HashMap<(String, String), Entry>>,
    /// Hot-updatable so the server can resize it to the current admission capacity
    /// without rebuilding the registry; read in [`meter_for`](Self::meter_for).
    max_tracked: AtomicUsize,
    idle_evict_epochs: u32,
}

impl Default for TrafficRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TrafficRegistry {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            max_tracked: AtomicUsize::new(DEFAULT_MAX_TRACKED),
            idle_evict_epochs: DEFAULT_IDLE_EVICT_EPOCHS,
        }
    }

    /// Construct with explicit bounds (tests exercise the cap + eviction cheaply).
    pub fn with_limits(max_tracked: usize, idle_evict_epochs: u32) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            max_tracked: AtomicUsize::new(max_tracked.max(1)),
            idle_evict_epochs,
        }
    }

    /// Resize the entry ceiling at runtime (clamped to ≥1). The server calls this
    /// each flush with `2 × Σ(admitted keys' max_conns)` so the cap follows the
    /// hot-reloaded admission set. Lowering it never evicts existing entries — it
    /// just refuses *new* pairs until idle eviction brings the map back under.
    pub fn set_max_tracked(&self, max_tracked: usize) {
        self.max_tracked
            .store(max_tracked.max(1), Ordering::Relaxed);
    }

    /// The current entry ceiling (test/observability helper).
    pub fn max_tracked(&self) -> usize {
        self.max_tracked.load(Ordering::Relaxed)
    }

    /// A [`TrafficMeter`] for `(remote_api_key, server_id)` metering in `direction`.
    /// Reuses the pair's shared [`Counters`] (creating them on first use); once the
    /// map is at [`max_tracked`](Self::with_limits) a *new* pair gets an **inert**
    /// meter (records nothing) so a `relay_node_id` churn flood can't grow the map.
    /// Resolved once per leg (like `limiter_for`), never on the hot path.
    pub fn meter_for(
        &self,
        remote_api_key: &str,
        server_id: &str,
        direction: Direction,
    ) -> TrafficMeter {
        let key = (remote_api_key.to_string(), server_id.to_string());
        let cap = self.max_tracked.load(Ordering::Relaxed);
        let mut map = self.map.lock();
        let counters = if let Some(entry) = map.get(&key) {
            Some(Arc::clone(&entry.counters))
        } else if map.len() < cap {
            let counters = Arc::new(Counters::default());
            map.insert(
                key,
                Entry {
                    counters: Arc::clone(&counters),
                    flushed: Counts::default(),
                    pending: Counts::default(),
                    idle_epochs: 0,
                },
            );
            Some(counters)
        } else {
            None
        };
        TrafficMeter {
            counters,
            direction,
        }
    }

    /// Snapshot each entry once and return the non-zero deltas since the last
    /// [`commit`](Self::commit). Pure read of the atomics + a stash of the snapshot
    /// in `pending`; it does **not** advance the baseline or evict, so a caller whose
    /// downstream write fails can simply discard the result and re-collect next
    /// interval. No I/O is done under the lock.
    pub fn collect(&self) -> Vec<RelayTrafficDelta> {
        let mut map = self.map.lock();
        let mut out = Vec::new();
        for ((remote_api_key, server_id), entry) in map.iter_mut() {
            let live = entry.counters.load();
            let delta = live.delta_since(&entry.flushed);
            entry.pending = live;
            if !delta.is_zero() {
                out.push(RelayTrafficDelta {
                    remote_api_key: remote_api_key.clone(),
                    server_id: server_id.clone(),
                    counts: delta,
                });
            }
        }
        out
    }

    /// Promote the last [`collect`](Self::collect)'s `pending` snapshot to the
    /// persisted baseline (call **only after** that collect's deltas were durably
    /// written), roll the idle bookkeeping, and evict a fully-drained entry with no
    /// live leg. Eviction is safe: the `Arc` strong count being 1 means no leg can
    /// still increment the counters, and a later reactivation creates a fresh entry
    /// whose UPSERT keeps accumulating into the same DB row.
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
            // Keep any entry still in use or recently active. Only when it has been
            // idle long enough AND no leg holds the Arc do we consider dropping it.
            if entry.idle_epochs < evict_epochs || Arc::strong_count(&entry.counters) != 1 {
                return true;
            }
            // `strong_count == 1` (a Relaxed load) gives no happens-before with the
            // departed leg's final `record`; an Acquire fence pairs with the Arc-drop
            // Release so we observe the leg's last write before deciding to evict.
            fence(Ordering::Acquire);
            // Drop only if genuinely drained (a frame that landed after `collect`
            // leaves live > flushed → keep it for the next interval).
            entry.counters.load() != entry.flushed
        });
    }

    /// Cumulative-since-start counts per tracked entry (for tests / a future
    /// dashboard). Distinct from [`collect`](Self::collect)'s per-interval deltas.
    pub fn snapshot(&self) -> Vec<RelayTrafficDelta> {
        let map = self.map.lock();
        map.iter()
            .map(|((remote_api_key, server_id), entry)| RelayTrafficDelta {
                remote_api_key: remote_api_key.clone(),
                server_id: server_id.clone(),
                counts: entry.counters.load(),
            })
            .collect()
    }

    /// Number of entries currently tracked (test/observability helper).
    pub fn tracked_len(&self) -> usize {
        self.map.lock().len()
    }
}

/// A cheap, cloneable handle on one leg's shared [`Counters`] plus the direction its
/// frames meter as. An *inert* meter (`counters: None`, handed out past the registry
/// cap) silently records nothing.
#[derive(Clone)]
pub struct TrafficMeter {
    counters: Option<Arc<Counters>>,
    direction: Direction,
}

impl TrafficMeter {
    /// Account one forwarded frame of `nbytes`. The relay hot path: at most two
    /// `fetch_add(Relaxed)` on the shared counters, no lock, no allocation.
    #[inline]
    pub fn record(&self, nbytes: usize) {
        let Some(counters) = &self.counters else {
            return;
        };
        let n = nbytes as u64;
        match self.direction {
            Direction::Up => {
                counters.bytes_up.fetch_add(n, Ordering::Relaxed);
                counters.frames_up.fetch_add(1, Ordering::Relaxed);
            }
            Direction::Down => {
                counters.bytes_down.fetch_add(n, Ordering::Relaxed);
                counters.frames_down.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Per-content-leg metering handed to the pump: the shared bandwidth limiter
/// ([`throttle`](BandwidthLimiter::throttle)) and the traffic meter
/// ([`record`](TrafficMeter::record)). Bundled so the pump takes one optional
/// argument; pairing/control legs pass `None` (they carry tiny, unmetered frames).
pub struct LegMetering {
    pub limiter: BandwidthLimiter,
    pub meter: TrafficMeter,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> TrafficRegistry {
        TrafficRegistry::new()
    }

    #[test]
    fn record_accumulates_per_direction() {
        let reg = reg();
        let up = reg.meter_for("key", "srv", Direction::Up);
        let down = reg.meter_for("key", "srv", Direction::Down);
        up.record(100);
        up.record(50);
        down.record(200);

        let snap = reg.snapshot();
        assert_eq!(
            snap.len(),
            1,
            "both directions share one (key, server) entry"
        );
        let c = snap[0].counts;
        assert_eq!(c.bytes_up, 150);
        assert_eq!(c.frames_up, 2);
        assert_eq!(c.bytes_down, 200);
        assert_eq!(c.frames_down, 1);
    }

    #[test]
    fn collect_then_commit_yields_zero_on_the_second_collect() {
        let reg = reg();
        let m = reg.meter_for("k", "s", Direction::Up);
        m.record(64);

        let first = reg.collect();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].counts.bytes_up, 64);
        reg.commit();

        // No new traffic since the commit advanced the baseline.
        assert!(reg.collect().is_empty(), "drained entry has no fresh delta");
        // A further frame shows up as a fresh delta off the advanced baseline.
        m.record(36);
        let second = reg.collect();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].counts.bytes_up, 36, "delta is since-last-commit");
    }

    #[test]
    fn a_record_between_collect_and_commit_is_not_lost() {
        let reg = reg();
        let m = reg.meter_for("k", "s", Direction::Up);
        m.record(10);
        let first = reg.collect();
        assert_eq!(first[0].counts.bytes_up, 10);
        // A frame lands after collect snapshotted but before commit advances.
        m.record(7);
        reg.commit();
        // The post-collect byte is captured in the next interval, not folded away.
        let second = reg.collect();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].counts.bytes_up, 7);
    }

    #[test]
    fn restart_re_accumulates_from_zero() {
        // A fresh registry (process restart: in-memory counters reset to 0) starts
        // its deltas from zero — the durable DB total resumes via UPSERT-accumulate.
        let reg = reg();
        let m = reg.meter_for("k", "s", Direction::Down);
        m.record(500);
        let d = reg.collect();
        assert_eq!(
            d[0].counts.bytes_down, 500,
            "first delta is the full session"
        );
    }

    #[test]
    fn eviction_drops_a_dead_drained_entry_but_keeps_an_open_one() {
        let reg = TrafficRegistry::with_limits(64, 1);
        // Open leg: holds the meter Arc → never evicted while live.
        let open = reg.meter_for("live", "s", Direction::Up);
        open.record(1);
        // Dead leg: meter dropped, so its entry has no live holder.
        {
            let dead = reg.meter_for("dead", "s", Direction::Up);
            dead.record(1);
        }
        assert_eq!(reg.tracked_len(), 2);

        // Interval 1: both have a delta → idle_epochs reset to 0, neither evicted.
        reg.collect();
        reg.commit();
        assert_eq!(reg.tracked_len(), 2);
        // Interval 2: no new traffic → idle_epochs hits the threshold. The dead,
        // drained entry is reclaimed; the open one is kept (its Arc is still held).
        reg.collect();
        reg.commit();
        assert_eq!(
            reg.tracked_len(),
            1,
            "only the dead drained entry is evicted"
        );
        assert_eq!(reg.snapshot()[0].remote_api_key, "live");
        // The live leg can still record after its partner's slot was reclaimed.
        open.record(9);
        assert_eq!(reg.snapshot()[0].counts.bytes_up, 10);
    }

    #[test]
    fn at_cap_a_new_pair_gets_an_inert_meter() {
        let reg = TrafficRegistry::with_limits(1, 5);
        let a = reg.meter_for("a", "s", Direction::Up);
        a.record(5);
        // The map is full; a brand-new pair is handed an inert meter that records
        // nothing rather than growing the map.
        let b = reg.meter_for("b", "s", Direction::Up);
        b.record(123);
        assert_eq!(reg.tracked_len(), 1, "the cap is enforced");
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].remote_api_key, "a");
        // An *existing* pair is still metered even at the cap.
        let a2 = reg.meter_for("a", "s", Direction::Down);
        a2.record(2);
        assert_eq!(reg.snapshot()[0].counts.bytes_down, 2);
    }

    #[test]
    fn set_max_tracked_resizes_the_cap_at_runtime() {
        let reg = TrafficRegistry::with_limits(1, 5);
        reg.meter_for("a", "s", Direction::Up).record(1);
        // At cap 1 a new pair is inert.
        reg.meter_for("b", "s", Direction::Up).record(9);
        assert_eq!(reg.tracked_len(), 1);
        // Raise the cap → the new pair is now tracked.
        reg.set_max_tracked(8);
        assert_eq!(reg.max_tracked(), 8);
        reg.meter_for("b", "s", Direction::Up).record(9);
        assert_eq!(reg.tracked_len(), 2);
        // Lowering the cap (clamped ≥1) does NOT evict existing entries; it only
        // refuses new pairs until idle eviction brings the map back under.
        reg.set_max_tracked(0);
        assert_eq!(reg.max_tracked(), 1);
        assert_eq!(
            reg.tracked_len(),
            2,
            "existing entries kept when the cap drops"
        );
        reg.meter_for("c", "s", Direction::Up).record(1);
        assert_eq!(
            reg.tracked_len(),
            2,
            "a new pair is refused under the low cap"
        );
    }

    #[test]
    fn distinct_keys_are_independent() {
        let reg = reg();
        reg.meter_for("a", "s", Direction::Up).record(10);
        reg.meter_for("b", "s", Direction::Up).record(20);
        let mut snap = reg.snapshot();
        snap.sort_by(|x, y| x.remote_api_key.cmp(&y.remote_api_key));
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].counts.bytes_up, 10);
        assert_eq!(snap[1].counts.bytes_up, 20);
    }
}
