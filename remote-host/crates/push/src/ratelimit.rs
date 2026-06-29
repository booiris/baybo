//! Per-`device_id` frequency control for `POST /notify`.
//!
//! A gateway POSTs `/notify` on every pushable turn; this bounds how fast a
//! single device can be pushed to, so a buggy or abusive gateway can neither
//! hammer APNs nor spam a phone. The key is `device_id` — a 32-byte Ed25519
//! public key, globally unique — so the budget follows the physical device and
//! one chatty device can't starve another. Because `/notify` is signature-checked
//! against the device's delegated gateway key *before* this runs, a notify for a
//! device the caller doesn't own can't reach (and thus can't drain) its bucket.
//! Over the limit, the caller returns `429`.
//!
//! Each device gets a [`TokenBucket`]; an over-cap soft limit evicts brim-full
//! (idle) buckets so a churn of device ids can't grow the map without bound. The
//! rate is configurable (defaults below); time is injectable for deterministic
//! tests.

use std::collections::HashMap;
use std::time::Instant;

use parking_lot::Mutex;
use remote_host_ratelimit::TokenBucket;

/// Sustained `/notify` rate per device, in pushes **per minute** — the natural
/// granularity for a notification (pushes are sparse, one per pushable turn).
/// Human-paced chat stays well under it; a runaway gateway is clamped here.
pub const NOTIFY_RATE_PER_MIN: f64 = 60.0;

/// Per-device `/notify` burst — pushes that may land back-to-back before pacing
/// kicks in (e.g. a flurry at session start).
pub const NOTIFY_BURST: f64 = 20.0;

/// Seconds per minute — converts [`NOTIFY_RATE_PER_MIN`] to the token bucket's
/// per-second refill.
const SECS_PER_MIN: f64 = 60.0;

/// The notify-limiter's bucket-map soft cap is this multiple of the **configured**
/// device-store cap (see [`NotifyRateLimiter::for_store_cap`]). The limiter only
/// ever mints a bucket for a *registered* device, so the live set can't exceed the
/// store; this headroom for lingering churned-out buckets keeps the O(n) eviction
/// scan off the hot path — a pathological-churn backstop, not a routine path.
const BUCKET_CAP_STORE_MULTIPLE: usize = 2;

/// One token-bucket per `device_id`, all at the configured rate. `soft_cap` bounds
/// the map; when exceeded, brim-full (idle) buckets are evicted — dropping a full
/// bucket grants no extra allowance (a re-created one also starts full), so
/// eviction is free of fairness risk.
pub struct NotifyRateLimiter {
    buckets: Mutex<HashMap<String, TokenBucket>>,
    burst: f64,
    refill_per_sec: f64,
    soft_cap: usize,
}

impl Default for NotifyRateLimiter {
    /// The built-in [`NOTIFY_RATE_PER_MIN`] / [`NOTIFY_BURST`] rate, sized for the
    /// default device-store cap.
    fn default() -> Self {
        Self::for_store_cap(
            NOTIFY_RATE_PER_MIN,
            NOTIFY_BURST,
            crate::store::DEVICE_STORE_SOFT_CAP,
        )
    }
}

impl NotifyRateLimiter {
    /// `rate_per_min` sustained pushes/min and `burst` back-to-back pushes per
    /// device; the bucket-map soft cap is derived from `device_store_cap` (the
    /// runtime-configured store size) via [`BUCKET_CAP_STORE_MULTIPLE`]. The
    /// operator overrides the rate + store cap via env (see `PushLimits`).
    pub fn for_store_cap(rate_per_min: f64, burst: f64, device_store_cap: usize) -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            burst,
            refill_per_sec: rate_per_min / SECS_PER_MIN,
            soft_cap: device_store_cap * BUCKET_CAP_STORE_MULTIPLE,
        }
    }

    /// Account one push for `device_id`; returns whether it is within the rate. A
    /// `false` means the caller should refuse with `429`.
    pub fn check(&self, device_id: &str) -> bool {
        self.check_at(device_id, Instant::now())
    }

    fn check_at(&self, device_id: &str, now: Instant) -> bool {
        let mut buckets = self.buckets.lock();
        if buckets.len() >= self.soft_cap {
            buckets.retain(|_, b| !b.is_full_at(now));
        }
        buckets
            .entry(device_id.to_string())
            .or_insert_with(|| TokenBucket::new_at(self.burst, self.refill_per_sec, now))
            .try_take_at(1.0, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn allows_the_burst_then_throttles_then_recovers() {
        let limiter = NotifyRateLimiter::default();
        let t0 = Instant::now();
        // The whole burst lands back-to-back in the same instant.
        for _ in 0..NOTIFY_BURST as usize {
            assert!(limiter.check_at("dev-1", t0));
        }
        // The next one over the burst is refused.
        assert!(!limiter.check_at("dev-1", t0), "burst exhausted -> 429");
        // One token refills after a second.
        assert!(limiter.check_at("dev-1", t0 + Duration::from_secs(1)));
        assert!(!limiter.check_at("dev-1", t0 + Duration::from_secs(1)));
    }

    #[test]
    fn buckets_are_per_device() {
        let limiter = NotifyRateLimiter::default();
        let t0 = Instant::now();
        // Drain dev-1 to its burst.
        for _ in 0..NOTIFY_BURST as usize {
            assert!(limiter.check_at("dev-1", t0));
        }
        assert!(!limiter.check_at("dev-1", t0), "dev-1 drained");
        // A different device has its own budget.
        assert!(limiter.check_at("dev-2", t0));
    }

    #[test]
    fn a_custom_rate_is_honored() {
        // A tighter rate (10/min, burst 3) clamps sooner than the default.
        let limiter = NotifyRateLimiter::for_store_cap(10.0, 3.0, 1000);
        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(limiter.check_at("dev-1", t0));
        }
        assert!(!limiter.check_at("dev-1", t0), "burst 3 exhausted");
    }
}
