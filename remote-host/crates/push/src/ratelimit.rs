//! Per-`device_id` frequency control for `POST /notify`.
//!
//! A gateway POSTs `/notify` on every pushable turn; this bounds how fast a
//! single device can be pushed to, so a buggy or abusive gateway can neither
//! hammer APNs nor spam a phone. The key is `device_id` alone — a 32-byte Ed25519
//! public key, globally unique — so the budget follows the physical device, and
//! one chatty device can't starve another. Because `/notify` is signature-checked
//! against the device's delegated gateway key *before* this runs, a co-tenant
//! sharing the `remote_api_key` can't reach (and thus can't drain) another
//! device's bucket. Over the limit, the caller returns `429`.
//!
//! The rate is fixed ([`NOTIFY_RATE_PER_MIN`] sustained, [`NOTIFY_BURST`] burst),
//! not configurable. Each device gets a [`TokenBucket`]; an over-cap soft limit
//! evicts brim-full (idle) buckets so a churn of device ids can't grow the map
//! without bound. Time is injectable for deterministic tests.

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

/// Soft cap on tracked device buckets. When exceeded, brim-full (idle) buckets
/// are evicted — dropping a full bucket grants no extra allowance (a re-created
/// one also starts full), so eviction is free of fairness risk.
const BUCKET_SOFT_CAP: usize = 16_384;

/// One token-bucket per `device_id`, all at the fixed rate.
#[derive(Default)]
pub struct NotifyRateLimiter {
    buckets: Mutex<HashMap<String, TokenBucket>>,
}

impl NotifyRateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Account one push for `device_id`; returns whether it is within the rate. A
    /// `false` means the caller should refuse with `429`.
    pub fn check(&self, device_id: &str) -> bool {
        self.check_at(device_id, Instant::now())
    }

    fn check_at(&self, device_id: &str, now: Instant) -> bool {
        let mut buckets = self.buckets.lock();
        if buckets.len() >= BUCKET_SOFT_CAP {
            buckets.retain(|_, b| !b.is_full_at(now));
        }
        buckets
            .entry(device_id.to_string())
            .or_insert_with(|| {
                TokenBucket::new_at(NOTIFY_BURST, NOTIFY_RATE_PER_MIN / SECS_PER_MIN, now)
            })
            .try_take_at(1.0, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn allows_the_burst_then_throttles_then_recovers() {
        let limiter = NotifyRateLimiter::new();
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
        let limiter = NotifyRateLimiter::new();
        let t0 = Instant::now();
        // Drain dev-1 to its burst.
        for _ in 0..NOTIFY_BURST as usize {
            assert!(limiter.check_at("dev-1", t0));
        }
        assert!(!limiter.check_at("dev-1", t0), "dev-1 drained");
        // A different device has its own budget.
        assert!(limiter.check_at("dev-2", t0));
    }
}
