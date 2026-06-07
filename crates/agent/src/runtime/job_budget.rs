//! Global concurrency budget for background subagent dispatches.
//!
//! A background subagent's child actor is built immediately but parks on
//! its mailbox — doing no LLM work — until it can [`JobBudget::acquire`] a
//! permit and is then fed its prompt. So this budget bounds how many
//! background children *run* at once, process-wide. Over the budget,
//! `acquire` waits on the `Semaphore`'s FIFO queue, realising the
//! "queue background dispatches when full" behaviour without a separate
//! data structure.
//!
//! Foreground subagents (the caller blocks on them) and detached `Bash`
//! commands (already running by the time they detach) are NOT gated by
//! this — see `docs/todo/job-pool.md`.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub struct JobBudget {
    semaphore: Arc<Semaphore>,
    total: usize,
}

impl JobBudget {
    pub fn new(total: usize) -> Arc<Self> {
        Arc::new(Self {
            semaphore: Arc::new(Semaphore::new(total)),
            total,
        })
    }

    /// Acquire a permit, queuing (FIFO) until one frees. The returned
    /// permit is held for the child's running lifetime; dropping it
    /// releases the slot for the next queued dispatch. `None` only if the
    /// semaphore was closed, which the runtime never does.
    pub async fn acquire(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.semaphore).acquire_owned().await.ok()
    }

    /// Total budget (the configured `max_concurrent_background_jobs`).
    pub fn total(&self) -> usize {
        self.total
    }

    /// Background children currently running (holding a permit). The
    /// difference between this and the registry's in-flight count is the
    /// number queued (built, awaiting a slot).
    pub fn running(&self) -> usize {
        self.total
            .saturating_sub(self.semaphore.available_permits())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn budget_caps_concurrent_holders_and_queues_the_rest() {
        let budget = JobBudget::new(2);
        let p1 = budget.acquire().await.expect("permit 1");
        let _p2 = budget.acquire().await.expect("permit 2");
        assert_eq!(budget.running(), 2);
        assert_eq!(budget.total(), 2);

        // The budget is full: a third acquire must not resolve until a
        // permit frees.
        let mut third = Box::pin(budget.acquire());
        assert!(
            futures::poll!(&mut third).is_pending(),
            "third acquire must queue while the budget is full"
        );

        drop(p1);
        let _p3 = third.await.expect("permit 3 after release");
        assert_eq!(budget.running(), 2, "still 2 holders (p2 + p3)");
    }
}
