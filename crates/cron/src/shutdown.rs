//! Shutdown abstraction used by [`crate::CronScheduler::run`].
//!
//! The scheduler must be able to wait on an external "shutdown requested"
//! signal without depending on any specific shutdown type — that coupling
//! belongs to the binary that owns process lifecycle (`baybo-agent` /
//! `crates/baybo/src/main.rs`). Implementers supply the concrete semantics and the
//! scheduler selects on `wait()` inside its tick loop.

use async_trait::async_trait;

/// Waitable shutdown handle. Scheduler drivers hold an
/// [`std::sync::Arc<dyn Shutdown>`] and `.wait().await` for the request.
#[async_trait]
pub trait Shutdown: Send + Sync {
    /// Resolves when shutdown has been requested. Implementations must be
    /// safe to call from multiple tasks concurrently.
    async fn wait(&self);

    /// Non-blocking check: has shutdown been requested already?
    fn is_triggered(&self) -> bool;
}

/// [`Shutdown`] that never fires — useful for tests that only invoke
/// scheduler methods directly without entering [`crate::CronScheduler::run`].
///
/// Gated behind `cfg(test)` (for this crate) and the `test-support` feature
/// (for downstream crates' tests). Not present in production builds.
#[cfg(any(test, feature = "test-support"))]
pub struct NeverShutdown;

#[cfg(any(test, feature = "test-support"))]
#[async_trait]
impl Shutdown for NeverShutdown {
    async fn wait(&self) {
        std::future::pending::<()>().await;
    }

    fn is_triggered(&self) -> bool {
        false
    }
}
