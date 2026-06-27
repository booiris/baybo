//! Per-client-IP connection-rate limiting for the relay's WS routes.
//!
//! The per-rendezvous [`JoinRateLimiter`](crate::serve) throttles leg-stealing on
//! *one* rendezvous, and the per-`remote_api_key` connection cap
//! ([`ConnectionRegistry`](crate::conns)) bounds an *admitted* gateway — but
//! neither bounds a single host spraying *connection upgrades* across many
//! rendezvous/node ids (or failing admission on each), which can still churn
//! accept/upgrade work. This is the coarse outer backstop: a token bucket per
//! source IP, drawn one token per WS-upgrade attempt, applied ahead of admission
//! so even unadmitted floods are shed cheaply.
//!
//! **Deployment caveat.** By default the key is the *socket* peer IP. With
//! remote-host terminating TLS itself that is the real client (or its NAT). Behind
//! a proxy (e.g. Cloudflare) the peer is the proxy's edge, so every client would
//! share one bucket. Two postures fix that, configured at [`build_router`](crate::serve::build_router)
//! via [`IpLimitConfig`](crate::serve::IpLimitConfig): disable the limiter and
//! rate-limit at the proxy, **or** give it the trusted client-IP header(s) the
//! proxy sets (e.g. `cf-connecting-ip`) so it keys on the real client. Trust such
//! a header **only** when the origin is reachable solely via that proxy — it is
//! otherwise forgeable. The limiter is skipped for a request whose client IP can't
//! be resolved (no trusted header *and* no client-address info, e.g. unit tests).
//!
//! Mirrors [`NotifyRateLimiter`](../../push/src/ratelimit.rs): one [`TokenBucket`]
//! per key at a fixed rate, with brim-full (idle) buckets evicted over a soft cap
//! so a churn of distinct source IPs can't grow the map without bound. Time is
//! injectable so the math is tested without real sleeps.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Instant;

use parking_lot::Mutex;
use remote_host_ratelimit::TokenBucket;

/// Sustained relay-upgrade rate per source IP, in attempts **per second**.
/// Generous for a real client (the app's pair/content connect-retry loops sit
/// well under it) yet tight enough to clamp a flood from one host.
pub const IP_RATE_PER_SEC: f64 = 10.0;

/// Per-IP upgrade burst — attempts that may land back-to-back before pacing
/// kicks in (the app's retry loop and a content session opening in quick
/// succession stay inside it).
pub const IP_BURST: f64 = 60.0;

/// Soft cap on tracked source-IP buckets. When exceeded, brim-full (idle)
/// buckets are evicted — dropping a full bucket grants no extra allowance (a
/// re-created one also starts full), so eviction is free of fairness risk.
const IP_BUCKET_SOFT_CAP: usize = 16_384;

/// One token-bucket per source IP, all at the fixed relay-upgrade rate.
#[derive(Default)]
pub struct IpRateLimiter {
    buckets: Mutex<HashMap<IpAddr, TokenBucket>>,
}

impl IpRateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Account one upgrade attempt for `ip`; returns whether it is within the
    /// rate. A `false` means the caller should refuse with `429`.
    pub fn check(&self, ip: IpAddr) -> bool {
        self.check_at(ip, Instant::now())
    }

    fn check_at(&self, ip: IpAddr, now: Instant) -> bool {
        let mut buckets = self.buckets.lock();
        if buckets.len() >= IP_BUCKET_SOFT_CAP {
            buckets.retain(|_, b| !b.is_full_at(now));
        }
        buckets
            .entry(ip)
            .or_insert_with(|| TokenBucket::new_at(IP_BURST, IP_RATE_PER_SEC, now))
            .try_take_at(1.0, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([127, 0, 0, n])
    }

    #[test]
    fn allows_the_burst_then_throttles_then_recovers() {
        let limiter = IpRateLimiter::new();
        let t0 = Instant::now();
        // The whole burst lands back-to-back in the same instant.
        for _ in 0..IP_BURST as usize {
            assert!(limiter.check_at(ip(1), t0));
        }
        // The next one over the burst is refused.
        assert!(!limiter.check_at(ip(1), t0), "burst exhausted -> 429");
        // Tokens refill at the sustained rate.
        assert!(limiter.check_at(ip(1), t0 + Duration::from_secs(1)));
    }

    #[test]
    fn buckets_are_per_ip() {
        let limiter = IpRateLimiter::new();
        let t0 = Instant::now();
        for _ in 0..IP_BURST as usize {
            assert!(limiter.check_at(ip(1), t0));
        }
        assert!(!limiter.check_at(ip(1), t0), "ip(1) drained");
        // A different source IP has its own independent budget.
        assert!(limiter.check_at(ip(2), t0), "distinct IP unaffected");
    }
}
