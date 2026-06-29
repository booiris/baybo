//! Frequency control for `POST /notify`, at two independent granularities:
//!
//! - **Per-`device_id`** ([`NotifyRateLimiter`]) bounds how fast a single device
//!   can be pushed to, so a buggy or abusive gateway can neither hammer APNs for
//!   one device nor spam a phone. The key is `device_id` (a 32-byte Ed25519 public
//!   key, globally unique), so the budget follows the physical device and one
//!   chatty device can't starve another. Because `/notify` is signature-checked
//!   against the device's delegated gateway key *before* this runs, a co-tenant
//!   sharing the `remote_api_key` can't drain another device's bucket.
//! - **Per-`remote_api_key`** ([`PerKeySendLimiter`]) is the aggregate APNs send
//!   ceiling: it caps the total real POST rate one key drives to the shared `.p8`,
//!   **regardless of how many `device_id`s that key registered**. Under the public
//!   `guest` key an attacker forges unlimited self-owned devices (each with a valid
//!   signing key), so the per-device bucket alone leaves the aggregate egress
//!   unbounded; this ceiling bounds it so one key-holder can't monopolize (and get
//!   Apple to penalize) the shared provider credential.
//!
//! Both ride the same [`KeyedRateLimiter`]: one [`TokenBucket`] per key at a fixed
//! rate, with brim-full (idle) buckets evicted over a soft cap so a churn of keys
//! can't grow the map without bound. Over the limit, the caller returns `429`.
//! Time is injectable for deterministic tests.

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

/// Sustained aggregate APNs send rate per `remote_api_key`, in pushes **per
/// minute**. Sits far above human-paced, per-turn traffic for a key with many
/// real devices (incl. the shared `guest` key) yet bounds the total egress one key
/// drives to the shared `.p8`, so a flood of self-owned devices can't monopolize
/// it. ~20/sec sustained.
pub const PER_KEY_SEND_RATE_PER_MIN: f64 = 1200.0;

/// Per-`remote_api_key` send burst — pushes that may land back-to-back before the
/// aggregate ceiling paces the key (a legitimate flurry across a key's devices
/// stays inside it).
pub const PER_KEY_SEND_BURST: f64 = 600.0;

/// Seconds per minute — converts the per-minute rates to the token bucket's
/// per-second refill.
const SECS_PER_MIN: f64 = 60.0;

/// Default soft cap on tracked buckets (override via `PUSH_LIMITER_BUCKET_CAP`).
/// When exceeded, brim-full (idle) buckets are evicted — dropping a full bucket
/// grants no extra allowance (a re-created one also starts full), so eviction is
/// free of fairness risk. This bounds the limiter's own map against a churn of
/// distinct keys; it's an internal memory edge, not a per-caller allowance.
pub const BUCKET_SOFT_CAP: usize = 16_384;

// Compile-time guard: the aggregate per-key ceiling must clear the per-device
// limits by a wide margin, else a single device could trip the key ceiling.
const _: () = assert!(PER_KEY_SEND_BURST > NOTIFY_BURST);
const _: () = assert!(PER_KEY_SEND_RATE_PER_MIN > NOTIFY_RATE_PER_MIN);

/// A keyed collection of fixed-rate token buckets — one [`TokenBucket`] per key,
/// all at the same rate — backing both notify frequency controls. `soft_cap`
/// bounds the map's size against a churn of distinct keys.
struct KeyedRateLimiter {
    buckets: Mutex<HashMap<String, TokenBucket>>,
    burst: f64,
    refill_per_sec: f64,
    soft_cap: usize,
}

impl KeyedRateLimiter {
    fn new(burst: f64, refill_per_sec: f64, soft_cap: usize) -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            burst,
            refill_per_sec,
            soft_cap,
        }
    }

    fn check_at(&self, key: &str, now: Instant) -> bool {
        let mut buckets = self.buckets.lock();
        if buckets.len() >= self.soft_cap {
            buckets.retain(|_, b| !b.is_full_at(now));
        }
        buckets
            .entry(key.to_string())
            .or_insert_with(|| TokenBucket::new_at(self.burst, self.refill_per_sec, now))
            .try_take_at(1.0, now)
    }
}

/// Per-`device_id` `/notify` frequency control at a configurable device rate.
pub struct NotifyRateLimiter {
    inner: KeyedRateLimiter,
}

impl Default for NotifyRateLimiter {
    /// The built-in [`NOTIFY_RATE_PER_MIN`] / [`NOTIFY_BURST`] / [`BUCKET_SOFT_CAP`]
    /// defaults.
    fn default() -> Self {
        Self::new(NOTIFY_RATE_PER_MIN, NOTIFY_BURST, BUCKET_SOFT_CAP)
    }
}

impl NotifyRateLimiter {
    /// `rate_per_min` sustained pushes/min and `burst` back-to-back pushes, per
    /// device; `soft_cap` bounds the tracked-device map. The operator overrides the
    /// defaults via env (see `PushLimits`).
    pub fn new(rate_per_min: f64, burst: f64, soft_cap: usize) -> Self {
        Self {
            inner: KeyedRateLimiter::new(burst, rate_per_min / SECS_PER_MIN, soft_cap),
        }
    }

    /// Account one push for `device_id`; returns whether it is within the rate. A
    /// `false` means the caller should refuse with `429`.
    pub fn check(&self, device_id: &str) -> bool {
        self.inner.check_at(device_id, Instant::now())
    }
}

/// Per-`remote_api_key` aggregate APNs send ceiling at the fixed per-key rate.
pub struct PerKeySendLimiter {
    inner: KeyedRateLimiter,
}

impl Default for PerKeySendLimiter {
    fn default() -> Self {
        Self::new(BUCKET_SOFT_CAP)
    }
}

impl PerKeySendLimiter {
    /// The per-key rate is fixed ([`PER_KEY_SEND_RATE_PER_MIN`] / [`PER_KEY_SEND_BURST`]);
    /// `soft_cap` bounds the tracked-key map.
    pub fn new(soft_cap: usize) -> Self {
        Self {
            inner: KeyedRateLimiter::new(
                PER_KEY_SEND_BURST,
                PER_KEY_SEND_RATE_PER_MIN / SECS_PER_MIN,
                soft_cap,
            ),
        }
    }

    /// Account one APNs send for `remote_api_key`; returns whether it is within the
    /// aggregate ceiling. A `false` means the caller should refuse with `429`.
    pub fn check(&self, remote_api_key: &str) -> bool {
        self.inner.check_at(remote_api_key, Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn keyed(burst: f64, rate_per_min: f64) -> KeyedRateLimiter {
        KeyedRateLimiter::new(burst, rate_per_min / SECS_PER_MIN, BUCKET_SOFT_CAP)
    }

    #[test]
    fn allows_the_burst_then_throttles_then_recovers() {
        let limiter = keyed(NOTIFY_BURST, NOTIFY_RATE_PER_MIN);
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
    fn buckets_are_per_key() {
        let limiter = keyed(NOTIFY_BURST, NOTIFY_RATE_PER_MIN);
        let t0 = Instant::now();
        // Drain dev-1 to its burst.
        for _ in 0..NOTIFY_BURST as usize {
            assert!(limiter.check_at("dev-1", t0));
        }
        assert!(!limiter.check_at("dev-1", t0), "dev-1 drained");
        // A different key has its own budget.
        assert!(limiter.check_at("dev-2", t0));
    }

    #[test]
    fn soft_cap_evicts_idle_buckets_so_the_map_stays_bounded() {
        // A tiny cap: once full, the next distinct key triggers a retain that drops
        // the brim-full (idle) buckets, so the map can't grow without bound.
        let limiter = KeyedRateLimiter::new(NOTIFY_BURST, NOTIFY_RATE_PER_MIN / SECS_PER_MIN, 2);
        let t0 = Instant::now();
        assert!(limiter.check_at("a", t0));
        assert!(limiter.check_at("b", t0));
        // "a" and "b" each drew one token, so they are NOT brim-full at t0; the cap
        // is hit but nothing is evictable, so the map holds all three transiently.
        assert!(limiter.check_at("c", t0));
        assert_eq!(limiter.buckets.lock().len(), 3);
        // A second later "a"/"b"/"c" have refilled to the brim; the next distinct
        // key's retain sweeps the idle ones, bounding the map.
        let later = t0 + Duration::from_secs(1);
        assert!(limiter.check_at("d", later));
        assert!(
            limiter.buckets.lock().len() <= 2,
            "idle buckets evicted at the cap"
        );
    }

    #[test]
    fn per_key_limiter_caps_the_aggregate_then_recovers() {
        let limiter = PerKeySendLimiter::default();
        let t0 = Instant::now();
        for _ in 0..PER_KEY_SEND_BURST as usize {
            assert!(limiter.inner.check_at("inst-A", t0));
        }
        assert!(!limiter.inner.check_at("inst-A", t0), "key ceiling reached");
        // A different key has its own aggregate budget.
        assert!(limiter.inner.check_at("inst-B", t0));
        // Tokens refill at the sustained rate.
        assert!(
            limiter
                .inner
                .check_at("inst-A", t0 + Duration::from_secs(1))
        );
    }
}
