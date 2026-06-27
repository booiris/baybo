//! A monotonic-clock token bucket — the one rate-limiting primitive shared by
//! the push frequency control (per `(remote_api_key, device_id)`) and the relay
//! bandwidth throttle (per `remote_api_key`, and per `(remote_api_key, server)`).
//!
//! The bucket holds up to `capacity` tokens and refills at `refill_per_sec`.
//! Refill is lazy — computed from the elapsed monotonic time on each access — so
//! there is no background ticker and an idle bucket costs nothing. Two ways to
//! draw it down, for the two limiter styles:
//!
//! - [`try_take`](TokenBucket::try_take) — *reject* if short. Push: a `/notify`
//!   over the limit is refused (`429`), it doesn't wait.
//! - [`reserve`](TokenBucket::reserve) — *throttle*: always grants, letting the
//!   balance go negative, and returns how long the caller must sleep to pay the
//!   debt back. Relay: a frame always flows, paced to the rate via backpressure.
//!
//! The type is not internally synchronized (`&mut self`); a registry wraps it in
//! a `Mutex` per key. Time is injectable ([`try_take_at`](TokenBucket::try_take_at)
//! / [`reserve_at`](TokenBucket::reserve_at)) so the math is tested without real
//! sleeps.

use std::time::{Duration, Instant};

/// A lazily-refilling token bucket. `tokens` may go negative under
/// [`reserve`](Self::reserve) (a borrowed burst, repaid over time); refill never
/// lifts it above `capacity`.
#[derive(Debug)]
pub struct TokenBucket {
    capacity: f64,
    refill_per_sec: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    /// A bucket starting full (`capacity` tokens). `capacity` is floored at 0 and
    /// `refill_per_sec` at a tiny positive value, so a degenerate `0` rate
    /// throttles hard rather than dividing by zero.
    pub fn new(capacity: f64, refill_per_sec: f64) -> Self {
        Self::new_at(capacity, refill_per_sec, Instant::now())
    }

    /// [`new`](Self::new) with an injected `now` (tests).
    pub fn new_at(capacity: f64, refill_per_sec: f64, now: Instant) -> Self {
        let capacity = capacity.max(0.0);
        Self {
            capacity,
            refill_per_sec: refill_per_sec.max(f64::MIN_POSITIVE),
            tokens: capacity,
            last: now,
        }
    }

    /// Retune an existing bucket in place (an admission hot-reload changed the
    /// key's rate). Tokens are clamped to the new capacity; the accrued balance
    /// is otherwise kept so an in-flight session isn't handed a fresh burst.
    pub fn set_rate(&mut self, capacity: f64, refill_per_sec: f64) {
        self.capacity = capacity.max(0.0);
        self.refill_per_sec = refill_per_sec.max(f64::MIN_POSITIVE);
        if self.tokens > self.capacity {
            self.tokens = self.capacity;
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        self.last = now;
    }

    /// Take `n` tokens if at least `n` are available, returning whether it
    /// succeeded. Never goes negative — the reject path.
    pub fn try_take(&mut self, n: f64) -> bool {
        self.try_take_at(n, Instant::now())
    }

    /// [`try_take`](Self::try_take) with an injected `now` (tests).
    pub fn try_take_at(&mut self, n: f64, now: Instant) -> bool {
        self.refill(now);
        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }

    /// Always take `n` tokens (the balance may go negative) and return how long
    /// the caller must wait before the debt is paid off — `ZERO` if tokens were
    /// available. The throttle path: the frame always flows, paced by sleeping
    /// the returned delay.
    pub fn reserve(&mut self, n: f64) -> Duration {
        self.reserve_at(n, Instant::now())
    }

    /// [`reserve`](Self::reserve) with an injected `now` (tests).
    pub fn reserve_at(&mut self, n: f64, now: Instant) -> Duration {
        self.refill(now);
        self.tokens -= n;
        if self.tokens < 0.0 {
            Duration::from_secs_f64(-self.tokens / self.refill_per_sec)
        } else {
            Duration::ZERO
        }
    }

    /// Whether the bucket would be brim-full at `now` (idle long enough to fully
    /// refill). Used to evict idle buckets without mutating them — a re-created
    /// bucket starts full too, so dropping an idle one grants no extra allowance.
    pub fn is_full_at(&self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens + elapsed * self.refill_per_sec >= self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_take_rejects_when_empty_and_refills_over_time() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new_at(2.0, 1.0, t0); // cap 2, 1 token/sec
        assert!(b.try_take_at(1.0, t0), "1/2 available");
        assert!(b.try_take_at(1.0, t0), "2/2 available");
        assert!(!b.try_take_at(1.0, t0), "empty -> rejected");
        // One token refills after a second.
        assert!(b.try_take_at(1.0, t0 + Duration::from_secs(1)));
        assert!(
            !b.try_take_at(1.0, t0 + Duration::from_secs(1)),
            "only one accrued"
        );
    }

    #[test]
    fn refill_never_exceeds_capacity() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new_at(3.0, 1.0, t0);
        // Idle for an hour, then a burst can only spend up to capacity.
        let later = t0 + Duration::from_secs(3600);
        assert!(b.try_take_at(3.0, later));
        assert!(
            !b.try_take_at(0.5, later),
            "capped at capacity, not the full hour"
        );
    }

    #[test]
    fn reserve_paces_by_returning_the_debt_delay() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new_at(10.0, 10.0, t0); // 10 bytes/sec, burst 10
        // Burst of 10 is free (within capacity).
        assert_eq!(b.reserve_at(10.0, t0), Duration::ZERO);
        // Next 10 overdraws to -10 -> must wait 10/10 = 1s.
        assert_eq!(b.reserve_at(10.0, t0), Duration::from_secs(1));
        // A frame larger than capacity still flows, paced proportionally.
        let mut b2 = TokenBucket::new_at(10.0, 10.0, t0);
        assert_eq!(
            b2.reserve_at(50.0, t0),
            Duration::from_secs(4),
            "(50-10)/10"
        );
    }

    #[test]
    fn reserve_recovers_after_waiting() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new_at(10.0, 10.0, t0);
        assert_eq!(
            b.reserve_at(20.0, t0),
            Duration::from_secs(1),
            "overdrawn to -10"
        );
        // After the 1s debt is paid, the balance is back to zero and a fresh
        // capacity-sized draw is free again.
        assert_eq!(
            b.reserve_at(10.0, t0 + Duration::from_secs(2)),
            Duration::ZERO
        );
    }

    #[test]
    fn set_rate_clamps_tokens_to_new_capacity() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new_at(100.0, 100.0, t0); // full at 100
        b.set_rate(10.0, 10.0);
        assert!(b.try_take_at(10.0, t0), "clamped down to the new cap of 10");
        assert!(!b.try_take_at(0.5, t0), "not the old 100");
    }

    #[test]
    fn is_full_after_idle() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new_at(5.0, 5.0, t0);
        assert!(b.try_take_at(5.0, t0));
        assert!(!b.is_full_at(t0), "just drained");
        assert!(
            b.is_full_at(t0 + Duration::from_secs(1)),
            "refilled to the brim"
        );
    }
}
