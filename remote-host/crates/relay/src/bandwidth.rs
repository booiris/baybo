//! Per-instance relay-bandwidth throttling.
//!
//! A content session rides two spliced legs (the phone's and the gateway's). The
//! only authenticated identity on the relay is the **gateway** (its admitted
//! `instance_key`) — the phone leg is anonymous — so bandwidth is metered per
//! `instance_key`, aggregated across *all* of that gateway's content legs and
//! both directions. Both legs of a session draw on the **same** bucket
//! ([`limiter_for`](BandwidthRegistry::limiter_for) returns a shared handle), so
//! the cap bounds a gateway's total relay throughput, not each leg's.
//!
//! The rate defaults to [`RELAY_BYTES_PER_SEC`], overridable per gateway by the
//! admission `max_bps` column (hot-reloaded). Enforcement is *throttle, not
//! drop*: [`throttle`](BandwidthLimiter::throttle) reserves the frame's bytes and
//! sleeps off any debt before the pump forwards it, so the unread socket
//! backpressures its sender over TCP. Nothing is lost; a sustained sender is just
//! paced to the rate.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use remote_host_ratelimit::TokenBucket;

/// Default per-gateway relay bandwidth in bytes/sec, aggregated across all of a
/// gateway's content legs and both directions, for keys with no `max_bps` of
/// their own. 1 MiB/s is generous for chat + the occasional attachment while
/// bounding a runaway gateway. The bucket also holds one second of burst (the
/// same value), so a brief spike passes without stutter.
pub const RELAY_BYTES_PER_SEC: u64 = 1024 * 1024;

/// Registry of per-`instance_key` byte buckets. A gateway's first content leg
/// lazily creates its bucket; subsequent legs share it. The map is bounded by the
/// admitted set (a handful of entries), and [`forget`](Self::forget) drops a
/// key's bucket when it loses admission.
#[derive(Default)]
pub struct BandwidthRegistry {
    buckets: Mutex<HashMap<String, Arc<Mutex<TokenBucket>>>>,
}

impl BandwidthRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The shared limiter for `instance_key`, applying its `max_bps` override (or
    /// the hardcoded default). Lazily creates the key's bucket; re-tunes an
    /// existing one to the current rate so an admission hot-reload of `max_bps`
    /// takes effect on the next leg.
    pub fn limiter_for(&self, instance_key: &str, max_bps: Option<u64>) -> BandwidthLimiter {
        let rate = max_bps.unwrap_or(RELAY_BYTES_PER_SEC).max(1) as f64;
        let bucket = self
            .buckets
            .lock()
            .entry(instance_key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(TokenBucket::new(rate, rate))))
            .clone();
        bucket.lock().set_rate(rate, rate);
        BandwidthLimiter { bucket }
    }

    /// Drop the buckets of keys that lost admission (called from the same revoke
    /// hot-reload that kicks their connections). A live leg holds its own `Arc`
    /// clone, so its in-flight throttle is unaffected; only the registry slot is
    /// freed, and a re-admitted key gets a fresh bucket.
    pub fn forget(&self, revoked: &HashSet<String>) {
        if revoked.is_empty() {
            return;
        }
        let mut map = self.buckets.lock();
        for key in revoked {
            map.remove(key);
        }
    }
}

/// A shared handle on one gateway's byte bucket. Cloning is cheap (an `Arc`); all
/// clones for the same key meter against the same bucket.
#[derive(Clone)]
pub struct BandwidthLimiter {
    bucket: Arc<Mutex<TokenBucket>>,
}

impl BandwidthLimiter {
    /// Reserve `nbytes` and return the delay owed before forwarding (sync; the
    /// lock is never held across the sleep). Separated from [`throttle`](Self::throttle)
    /// so the rate math is testable without real time.
    fn reserve(&self, nbytes: usize) -> Duration {
        self.bucket.lock().reserve(nbytes as f64)
    }

    /// Account `nbytes` against the bucket and sleep off any debt, pacing the
    /// caller to the configured rate. Called per forwarded frame in the pump.
    pub async fn throttle(&self, nbytes: usize) {
        let delay = self.reserve(nbytes);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }

    /// [`reserve`](Self::reserve) at an injected instant, so tests exercise the
    /// shared-bucket accounting deterministically (no wall-clock refill between
    /// calls).
    #[cfg(test)]
    fn reserve_at(&self, nbytes: usize, now: std::time::Instant) -> Duration {
        self.bucket.lock().reserve_at(nbytes as f64, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// One second of burst, the bucket's full capacity.
    const BURST: usize = RELAY_BYTES_PER_SEC as usize;

    // `now` is captured before any bucket is created, so each bucket's `last`
    // (set at creation) is ≥ `now` and `reserve_at(_, now)` never refills — every
    // assertion below is pure accounting, no wall-clock dependence.

    #[test]
    fn both_legs_of_a_session_share_one_bucket() {
        let now = Instant::now();
        let reg = BandwidthRegistry::new();
        // The phone leg and the gateway leg resolve to the same instance key.
        let phone = reg.limiter_for("inst-A", None);
        let gateway = reg.limiter_for("inst-A", None);
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
        let a = reg.limiter_for("inst-A", None);
        let b = reg.limiter_for("inst-B", None);
        // Drain A entirely and push it into debt.
        assert_eq!(a.reserve_at(BURST, now), Duration::ZERO);
        assert!(!a.reserve_at(1, now).is_zero(), "A is now in debt");
        // inst-B has its own full bucket, untouched by A.
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
        let capped = reg.limiter_for("slow", Some(1000)); // 1000 B/s override
        let default = reg.limiter_for("fast", None); // the 1 MiB/s default
        // The capped key owes debt after just its 1000-byte burst...
        assert_eq!(capped.reserve_at(1000, now), Duration::ZERO);
        assert!(
            !capped.reserve_at(1, now).is_zero(),
            "capped key in debt at 1000 bytes"
        );
        // ...while the default key sails past the same amount.
        assert_eq!(default.reserve_at(1000, now), Duration::ZERO);
        assert_eq!(
            default.reserve_at(1, now),
            Duration::ZERO,
            "default unaffected"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn throttle_paces_an_overdraw_but_not_within_burst() {
        let reg = BandwidthRegistry::new();
        let limiter = reg.limiter_for("inst-A", None);
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
    fn forget_drops_a_revoked_keys_bucket() {
        let now = Instant::now();
        let reg = BandwidthRegistry::new();
        let leg = reg.limiter_for("gone", None);
        // Drive the live bucket into debt.
        let _ = leg.reserve_at(BURST + BURST, now);
        assert!(!leg.reserve_at(1, now).is_zero(), "in debt");
        // Revoke the key: a freshly resolved limiter starts from a clean bucket.
        reg.forget(&HashSet::from(["gone".to_string()]));
        let fresh = reg.limiter_for("gone", None);
        assert_eq!(
            fresh.reserve_at(1, now),
            Duration::ZERO,
            "a new bucket, no inherited debt"
        );
        // The pre-revoke handle keeps its own (now orphaned) bucket — unaffected.
        assert!(
            !leg.reserve_at(1, now).is_zero(),
            "the old leg's bucket lives until it drops"
        );
    }
}
