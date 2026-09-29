//! Punch pacing, shared by A and P: a burst is `PUNCH_BURST` rounds spaced by
//! `PUNCH_INTERVAL`, and each round sends one punch to each of the burst's
//! targets. The spacing covers the skew between the two sides' `Peer`
//! deliveries, and a later round passes the NAT an earlier one opened.

use std::time::Duration;

use tokio::time::{Instant, sleep_until};

pub const PUNCH_BURST: usize = 5;
pub const PUNCH_INTERVAL: Duration = Duration::from_millis(200);

/// Releases the rounds of one burst: the first at once, each later one
/// `PUNCH_INTERVAL` after the previous one was released. A caller that falls
/// behind gets its next round a full interval after the late one, never a
/// catch-up volley.
#[derive(Debug, Default)]
pub struct PunchBurst {
    released: usize,
    next_at: Option<Instant>,
}

impl PunchBurst {
    pub fn new() -> Self {
        Self::default()
    }

    /// Waits for the next round and returns its index, or `None` once every
    /// round has been released. Cancel safe: a cancelled wait releases nothing.
    pub async fn next_round(&mut self) -> Option<usize> {
        if self.released >= PUNCH_BURST {
            return None;
        }
        if let Some(at) = self.next_at {
            sleep_until(at).await;
        }
        let round = self.released;
        self.released += 1;
        self.next_at = Some(Instant::now() + PUNCH_INTERVAL);
        Some(round)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intervals(rounds: usize) -> Duration {
        PUNCH_INTERVAL * u32::try_from(rounds).unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_releases_its_rounds_at_the_punch_interval() {
        let start = Instant::now();
        let mut burst = PunchBurst::new();
        let mut released = Vec::new();
        while let Some(round) = burst.next_round().await {
            released.push((round, start.elapsed()));
        }
        let expected: Vec<_> = (0..PUNCH_BURST)
            .map(|round| (round, intervals(round)))
            .collect();
        assert_eq!(released, expected);
        assert_eq!(burst.next_round().await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_late_caller_gets_no_catch_up_volley() {
        let mut burst = PunchBurst::new();
        assert_eq!(burst.next_round().await, Some(0));
        tokio::time::advance(intervals(3)).await;

        let late = Instant::now();
        assert_eq!(burst.next_round().await, Some(1));
        assert_eq!(late.elapsed(), Duration::ZERO);
        assert_eq!(burst.next_round().await, Some(2));
        assert_eq!(late.elapsed(), PUNCH_INTERVAL);
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_wait_releases_nothing() {
        let start = Instant::now();
        let mut burst = PunchBurst::new();
        assert_eq!(burst.next_round().await, Some(0));
        assert!(
            tokio::time::timeout(PUNCH_INTERVAL / 2, burst.next_round())
                .await
                .is_err()
        );
        assert_eq!(burst.next_round().await, Some(1));
        assert_eq!(start.elapsed(), PUNCH_INTERVAL);
    }
}
