//! Two-level relay-bandwidth throttling, AND-enforced.
//!
//! A content session rides two spliced legs (the phone's and the gateway's). The
//! only authenticated identity on the relay is the **gateway** (its admitted
//! `remote_api_key` + the `server_id` it named) — the phone leg is anonymous — so
//! bandwidth is metered at two nested levels and every byte debits **both**:
//!
//! 1. a per-`remote_api_key` ceiling (`max_bps`), aggregated across *all* the
//!    key's content legs (every server, both directions) — the cross-tenant wall;
//! 2. a per-`(remote_api_key, server_id)` sub-cap (`per_server_max_bps`) — the
//!    "防自家互饿" anti-starvation bound so one of a key's own gateways can't eat
//!    the whole `max_bps`.
//!
//! A reserve debits both buckets and owes the **max** of the two debts, so neither
//! cap is ever exceeded (AND). Both legs of a session draw on the same bucket pair
//! ([`limiter_for`](BandwidthRegistry::limiter_for) returns a shared handle), so a
//! cap bounds total throughput, not each leg's.
//!
//! Rates default to [`RELAY_BYTES_PER_SEC`] (per key) and, when a key sets no
//! sub-cap, to the per-key rate (so the sub-cap is non-binding); both are
//! overridable per row via the admission `max_bps` / `per_server_max_bps` columns
//! (hot-reloaded). Enforcement is *throttle, not drop*:
//! [`throttle`](BandwidthLimiter::throttle) reserves a frame's bytes and sleeps off
//! any debt before the pump forwards it, so the unread socket backpressures its
//! sender over TCP. Nothing is lost; a sustained sender is just paced to the rate.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use remote_host_ratelimit::TokenBucket;

/// A byte bucket shared by every leg that meters against the same level.
type SharedBucket = Arc<Mutex<TokenBucket>>;

/// Default per-`remote_api_key` relay bandwidth in bytes/sec, aggregated across all
/// the key's content legs and both directions, for keys with no `max_bps` of their
/// own. 1 MiB/s is generous for chat + the occasional attachment while bounding a
/// runaway key. The bucket also holds one second of burst (the same value), so a
/// brief spike passes without stutter.
pub const RELAY_BYTES_PER_SEC: u64 = 1024 * 1024;

/// Registry of the two bucket levels. A key's first content leg lazily creates its
/// per-key bucket and the per-`(key, server)` bucket; subsequent legs for the same
/// pair share them. Both maps are bounded by the admitted set × its few servers,
/// and [`forget`](Self::forget) drops every bucket of a key that loses admission.
#[derive(Default)]
pub struct BandwidthRegistry {
    per_key_buckets: Mutex<HashMap<String, SharedBucket>>,
    per_server_buckets: Mutex<HashMap<(String, String), SharedBucket>>,
}

impl BandwidthRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The shared two-level limiter for `(remote_api_key, server_id)`, applying the
    /// key's `max_bps` ceiling and `per_server_max_bps` sub-cap (each falling back
    /// to its default). Lazily creates both buckets; re-tunes existing ones to the
    /// current rates so an admission hot-reload of either column takes effect on the
    /// next leg. A NULL sub-cap means "no extra limit beyond the per-key ceiling",
    /// so it resolves to the per-key rate and never binds tighter.
    pub fn limiter_for(
        &self,
        remote_api_key: &str,
        server_id: &str,
        max_bps: Option<u64>,
        per_server_max_bps: Option<u64>,
    ) -> BandwidthLimiter {
        let key_rate = max_bps.unwrap_or(RELAY_BYTES_PER_SEC).max(1) as f64;
        let server_rate = per_server_max_bps
            .or(max_bps)
            .unwrap_or(RELAY_BYTES_PER_SEC)
            .max(1) as f64;

        let per_key_bucket = self
            .per_key_buckets
            .lock()
            .entry(remote_api_key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(TokenBucket::new(key_rate, key_rate))))
            .clone();
        per_key_bucket.lock().set_rate(key_rate, key_rate);

        let per_server_bucket = self
            .per_server_buckets
            .lock()
            .entry((remote_api_key.to_string(), server_id.to_string()))
            .or_insert_with(|| Arc::new(Mutex::new(TokenBucket::new(server_rate, server_rate))))
            .clone();
        per_server_bucket.lock().set_rate(server_rate, server_rate);

        BandwidthLimiter {
            per_key_bucket,
            per_server_bucket,
        }
    }

    /// Drop every bucket (per-key and all per-server) of keys that lost admission
    /// (called from the same revoke hot-reload that kicks their connections). A
    /// live leg holds its own `Arc` clones, so its in-flight throttle is unaffected;
    /// only the registry slots are freed, and a re-admitted key gets fresh buckets.
    pub fn forget(&self, revoked: &HashSet<String>) {
        if revoked.is_empty() {
            return;
        }
        {
            let mut map = self.per_key_buckets.lock();
            for key in revoked {
                map.remove(key);
            }
        }
        {
            let mut map = self.per_server_buckets.lock();
            map.retain(|(k, _server), _| !revoked.contains(k));
        }
    }
}

/// A shared handle on one gateway leg's two nested byte buckets. Cloning is cheap
/// (two `Arc`s); all clones for the same `(key, server)` meter against the same
/// pair.
#[derive(Clone)]
pub struct BandwidthLimiter {
    per_key_bucket: SharedBucket,
    per_server_bucket: SharedBucket,
}

impl BandwidthLimiter {
    /// Reserve `nbytes` against **both** buckets synchronously (no await between, so
    /// the two debits share the same instant) and return the larger of the two
    /// debts — the delay owed before forwarding so neither cap is exceeded. Sync;
    /// no lock is held across the sleep. Separated from [`throttle`](Self::throttle)
    /// so the rate math is testable without real time.
    fn reserve(&self, nbytes: usize) -> Duration {
        let key_delay = self.per_key_bucket.lock().reserve(nbytes as f64);
        let server_delay = self.per_server_bucket.lock().reserve(nbytes as f64);
        key_delay.max(server_delay)
    }

    /// Account `nbytes` against both buckets and sleep off any debt, pacing the
    /// caller to the tighter of the two rates. Called per forwarded frame in the
    /// pump.
    pub async fn throttle(&self, nbytes: usize) {
        let delay = self.reserve(nbytes);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }

    /// [`reserve`](Self::reserve) at an injected instant, so tests exercise the
    /// two-level accounting deterministically (no wall-clock refill between calls).
    #[cfg(test)]
    fn reserve_at(&self, nbytes: usize, now: std::time::Instant) -> Duration {
        let key_delay = self.per_key_bucket.lock().reserve_at(nbytes as f64, now);
        let server_delay = self.per_server_bucket.lock().reserve_at(nbytes as f64, now);
        key_delay.max(server_delay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// One second of burst at the per-key default, the bucket's full capacity.
    const BURST: usize = RELAY_BYTES_PER_SEC as usize;

    // `now` is captured before any bucket is created, so each bucket's `last`
    // (set at creation) is ≥ `now` and `reserve_at(_, now)` never refills — every
    // assertion below is pure accounting, no wall-clock dependence.

    #[test]
    fn both_legs_of_a_session_share_one_bucket_pair() {
        let now = Instant::now();
        let reg = BandwidthRegistry::new();
        // The phone leg and the gateway leg resolve to the same (key, server).
        let phone = reg.limiter_for("key-A", "srv", None, None);
        let gateway = reg.limiter_for("key-A", "srv", None, None);
        // One leg drains the whole burst (exactly empties it — no debt yet).
        assert_eq!(phone.reserve_at(BURST, now), Duration::ZERO);
        // The partner leg's next byte overdraws the *shared* bucket into debt.
        assert!(
            !gateway.reserve_at(1, now).is_zero(),
            "the partner leg sees the shared debt"
        );
    }

    #[test]
    fn distinct_keys_get_independent_buckets() {
        let now = Instant::now();
        let reg = BandwidthRegistry::new();
        let a = reg.limiter_for("key-A", "srv", None, None);
        let b = reg.limiter_for("key-B", "srv", None, None);
        assert_eq!(a.reserve_at(BURST, now), Duration::ZERO);
        assert!(!a.reserve_at(1, now).is_zero(), "A is now in debt");
        assert_eq!(
            b.reserve_at(1, now),
            Duration::ZERO,
            "a different key has its own bucket"
        );
    }

    #[test]
    fn per_key_max_bps_caps_below_the_default() {
        let now = Instant::now();
        let reg = BandwidthRegistry::new();
        let capped = reg.limiter_for("slow", "srv", Some(1000), None); // 1000 B/s
        let default = reg.limiter_for("fast", "srv", None, None); // the 1 MiB/s default
        assert_eq!(capped.reserve_at(1000, now), Duration::ZERO);
        assert!(
            !capped.reserve_at(1, now).is_zero(),
            "capped key in debt at 1000 bytes"
        );
        assert_eq!(default.reserve_at(1000, now), Duration::ZERO);
        assert_eq!(
            default.reserve_at(1, now),
            Duration::ZERO,
            "default unaffected"
        );
    }

    /// A tight per-server sub-cap binds even when the per-key ceiling has plenty of
    /// room — the anti-starvation guarantee.
    #[test]
    fn per_server_sub_cap_binds_before_the_per_key_ceiling() {
        let now = Instant::now();
        let reg = BandwidthRegistry::new();
        let leg = reg.limiter_for("key", "srv", Some(1_000_000), Some(1000));
        assert_eq!(leg.reserve_at(1000, now), Duration::ZERO);
        assert!(
            !leg.reserve_at(1, now).is_zero(),
            "the per-server sub-cap binds though the per-key ceiling is nearly empty"
        );
    }

    /// The per-key ceiling bounds the aggregate across a key's servers, even when
    /// each server's own sub-cap still has room.
    #[test]
    fn per_key_ceiling_binds_across_servers() {
        let now = Instant::now();
        let reg = BandwidthRegistry::new();
        let s1 = reg.limiter_for("key", "srv-1", Some(1000), Some(1_000_000));
        let s2 = reg.limiter_for("key", "srv-2", Some(1000), Some(1_000_000));
        // srv-1 drains the shared per-key burst (1000).
        assert_eq!(s1.reserve_at(1000, now), Duration::ZERO);
        // srv-2 has a fresh per-server bucket, but the shared per-key ceiling is empty.
        assert!(
            !s2.reserve_at(1, now).is_zero(),
            "the per-key ceiling bounds the aggregate across servers"
        );
    }

    /// `reserve` owes the larger of the two debts regardless of which bucket is the
    /// tighter one (the one-in-debt-other-full case, both directions).
    #[test]
    fn reserve_owes_the_max_of_the_two_debts() {
        let now = Instant::now();
        let reg = BandwidthRegistry::new();
        // per-key roomy (1 MiB/s), per-server tight (100 B/s): 200 bytes overdraws
        // only the per-server bucket, yet reserve still owes that debt.
        let server_bound = reg.limiter_for("k1", "srv", Some(1_000_000), Some(100));
        assert!(
            !server_bound.reserve_at(200, now).is_zero(),
            "max picks up the per-server debt while per-key is full"
        );
        // per-key tight (100 B/s), per-server roomy: same 200 bytes now overdraws
        // only the per-key bucket, and reserve owes that one instead.
        let key_bound = reg.limiter_for("k2", "srv", Some(100), Some(1_000_000));
        assert!(
            !key_bound.reserve_at(200, now).is_zero(),
            "max picks up the per-key debt while per-server is full"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn throttle_paces_an_overdraw_but_not_within_burst() {
        let reg = BandwidthRegistry::new();
        let limiter = reg.limiter_for("key-A", "srv", None, None);
        // Draining the whole burst owes no debt -> throttle returns without sleeping.
        let start = tokio::time::Instant::now();
        limiter.throttle(BURST).await;
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "within burst -> no sleep"
        );
        // Overdrawing by one second of rate owes ~1s -> throttle sleeps it off
        // (paced), which start_paused advances as virtual time.
        let start = tokio::time::Instant::now();
        limiter.throttle(BURST).await;
        assert!(
            start.elapsed() >= Duration::from_millis(900),
            "overdraw past the burst is paced, not dropped"
        );
    }

    #[test]
    fn forget_drops_both_bucket_levels_for_a_revoked_key() {
        let now = Instant::now();
        let reg = BandwidthRegistry::new();
        let leg = reg.limiter_for("gone", "srv", None, None);
        // Drive both live buckets into debt.
        let _ = leg.reserve_at(BURST + BURST, now);
        assert!(!leg.reserve_at(1, now).is_zero(), "in debt");
        // Revoke the key: a freshly resolved limiter starts from clean buckets,
        // which only holds if BOTH the per-key and per-server slots were dropped
        // (a retained per-server bucket would carry its debt forward).
        reg.forget(&HashSet::from(["gone".to_string()]));
        let fresh = reg.limiter_for("gone", "srv", None, None);
        assert_eq!(
            fresh.reserve_at(1, now),
            Duration::ZERO,
            "new buckets, no inherited debt"
        );
        // The pre-revoke handle keeps its own (now orphaned) buckets — unaffected.
        assert!(
            !leg.reserve_at(1, now).is_zero(),
            "the old leg's buckets live until it drops"
        );
    }
}
