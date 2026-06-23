//! In-flight job cancellation registry.
//!
//! `JobLifecycle::cancel(job_id, …)` already flips the DB row to
//! `Cancelled`, but without this registry the in-flight execution
//! (LLM call, tool, subagent) never sees the cancel and runs to
//! completion before noticing. The registry maps `JobId →
//! CancellationToken` for currently-executing jobs; tripping the token
//! propagates through `AgentLoop::run`'s child tokens into
//! `tool_executor::execute` and the subagent dispatch path.
//!
//! The actor runs jobs serially through its mailbox, but the registry
//! is process-wide because external callers (`/v1/jobs/{id}/cancel`,
//! the parent's `JobLifecycle::cancel` for a subagent's child) need
//! to look up tokens by `JobId` without knowing which actor owns them.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use baybo_model::JobId;
use parking_lot::RwLock;
use tokio_util::sync::CancellationToken;

/// How often [`JobCancellationRegistry::wait_until_idle`] re-checks
/// registration while waiting. Short enough that the post-cancel anchor read
/// follows the turn's drain by at most this, with a tiny bounded busy-wait.
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Default)]
pub struct JobCancellationRegistry {
    tokens: RwLock<HashMap<JobId, CancellationToken>>,
}

impl JobCancellationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an in-flight job's cancellation token. Replacing the
    /// entry for a given `job_id` is a programmer error — `register`
    /// is paired with `unregister` via the [`JobCancellationGuard`] so
    /// double-registration shouldn't happen, but if it does we keep the
    /// newer token (the older one is presumed orphaned).
    pub fn register(&self, job_id: JobId, token: CancellationToken) {
        self.tokens.write().insert(job_id, token);
    }

    pub fn unregister(&self, job_id: &JobId) {
        self.tokens.write().remove(job_id);
    }

    /// Trip the in-flight token for `job_id` if registered. Returns
    /// `true` if a live token was found and tripped.
    pub fn cancel(&self, job_id: &JobId) -> bool {
        if let Some(token) = self.tokens.read().get(job_id) {
            token.cancel();
            true
        } else {
            false
        }
    }

    /// Whether `job_id` currently has a live (registered) token.
    pub fn is_registered(&self, job_id: &JobId) -> bool {
        self.tokens.read().contains_key(job_id)
    }

    /// Wait until `job_id` is no longer registered — i.e. its run's
    /// `with_job` scope exited and dropped the guard, which happens *after*
    /// the agent loop's body (so after any partial assistant row it persists
    /// on a cancel). Bounded by `timeout`; returns immediately if already
    /// gone. `/stop` uses this so its control events anchor after the
    /// cancelled turn's last row instead of racing it.
    pub async fn wait_until_idle(&self, job_id: &JobId, timeout: Duration) {
        let _ = tokio::time::timeout(timeout, async {
            while self.is_registered(job_id) {
                tokio::time::sleep(IDLE_POLL_INTERVAL).await;
            }
        })
        .await;
    }
}

/// RAII guard returned by `JobLifecycle::register_running`. Drops
/// the registration on scope exit so an early `?` from the agent
/// loop can't leak a never-unregistered token.
#[must_use = "the guard unregisters the job on drop; bind it for the lifetime of the run"]
pub struct JobCancellationGuard {
    registry: Arc<JobCancellationRegistry>,
    job_id: JobId,
}

impl JobCancellationGuard {
    pub(crate) fn new(registry: Arc<JobCancellationRegistry>, job_id: JobId) -> Self {
        Self { registry, job_id }
    }
}

impl Drop for JobCancellationGuard {
    fn drop(&mut self) {
        self.registry.unregister(&self.job_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_unregistered_returns_false() {
        let r = JobCancellationRegistry::new();
        assert!(!r.cancel(&JobId::new()));
    }

    #[test]
    fn cancel_registered_trips_token() {
        let r = JobCancellationRegistry::new();
        let job_id = JobId::new();
        let token = CancellationToken::new();
        r.register(job_id, token.clone());
        assert!(!token.is_cancelled());
        assert!(r.cancel(&job_id));
        assert!(token.is_cancelled());
    }

    #[test]
    fn guard_unregisters_on_drop() {
        let r = Arc::new(JobCancellationRegistry::new());
        let job_id = JobId::new();
        let token = CancellationToken::new();
        r.register(job_id, token.clone());
        {
            let _guard = JobCancellationGuard::new(Arc::clone(&r), job_id);
            assert!(r.cancel(&job_id));
        }
        // After drop, the entry is gone — re-cancel reports no live token.
        assert!(!r.cancel(&job_id));
    }

    #[tokio::test]
    async fn wait_until_idle_returns_immediately_when_unregistered() {
        let r = JobCancellationRegistry::new();
        // Never registered → returns at once even with a generous budget.
        r.wait_until_idle(&JobId::new(), Duration::from_secs(5))
            .await;
    }

    #[tokio::test]
    async fn wait_until_idle_resolves_when_the_guard_drops() {
        let r = Arc::new(JobCancellationRegistry::new());
        let job_id = JobId::new();
        r.register(job_id, CancellationToken::new());
        let guard = JobCancellationGuard::new(Arc::clone(&r), job_id);

        let waiter = {
            let r = Arc::clone(&r);
            tokio::spawn(async move { r.wait_until_idle(&job_id, Duration::from_secs(30)).await })
        };
        // Still registered: the waiter must not have resolved.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!waiter.is_finished());

        drop(guard); // turn settled → deregistered
        tokio::time::sleep(Duration::from_millis(30)).await; // > IDLE_POLL_INTERVAL
        assert!(
            waiter.is_finished(),
            "wait must resolve once the guard drops"
        );
        waiter.await.expect("waiter task");
    }

    #[tokio::test]
    async fn wait_until_idle_gives_up_after_timeout() {
        let r = Arc::new(JobCancellationRegistry::new());
        let job_id = JobId::new();
        r.register(job_id, CancellationToken::new()); // never deregistered
        // Bounded: returns despite the entry never clearing (best-effort).
        r.wait_until_idle(&job_id, Duration::from_millis(40)).await;
        assert!(
            r.is_registered(&job_id),
            "entry still present; we just stopped waiting"
        );
    }
}
