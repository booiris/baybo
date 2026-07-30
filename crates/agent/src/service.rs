use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Coordinates graceful shutdown across all components.
#[derive(Clone)]
pub struct ShutdownSignal {
    token: CancellationToken,
}

impl ShutdownSignal {
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
        }
    }

    /// Check if shutdown has been requested.
    pub fn is_shutdown(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Trigger shutdown — wakes all waiters.
    pub fn trigger(&self) {
        info!("shutdown triggered");
        self.token.cancel();
    }

    /// Wait until shutdown is triggered.
    pub async fn wait(&self) {
        self.token.cancelled().await;
    }

    /// Clone the process-wide cancellation token for components that need to
    /// select shutdown alongside their own work.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.token.clone()
    }
}

impl Default for ShutdownSignal {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl baybo_cron::Shutdown for ShutdownSignal {
    async fn wait(&self) {
        ShutdownSignal::wait(self).await;
    }

    fn is_triggered(&self) -> bool {
        self.is_shutdown()
    }
}

/// Manages background task handles and ensures cleanup on shutdown.
pub struct TaskTracker {
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl TaskTracker {
    pub fn new() -> Self {
        Self {
            handles: Vec::new(),
        }
    }

    pub fn track(&mut self, handle: tokio::task::JoinHandle<()>) {
        self.handles.push(handle);
    }

    /// Abort all tracked tasks and wait for them to finish.
    pub async fn shutdown(self) {
        for handle in &self.handles {
            handle.abort();
        }
        for handle in self.handles {
            let _ = handle.await;
        }
    }
}

impl Default for TaskTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_signal_default_not_triggered() {
        let signal = ShutdownSignal::new();
        assert!(!signal.is_shutdown());
    }

    #[test]
    fn shutdown_signal_trigger() {
        let signal = ShutdownSignal::new();
        signal.trigger();
        assert!(signal.is_shutdown());
    }

    #[tokio::test]
    async fn shutdown_signal_wait_returns_immediately_if_triggered() {
        let signal = ShutdownSignal::new();
        signal.trigger();
        // Should not block
        signal.wait().await;
    }

    #[tokio::test]
    async fn shutdown_signal_clone_shares_state() {
        let s1 = ShutdownSignal::new();
        let s2 = s1.clone();
        let token = s2.cancellation_token();
        s1.trigger();
        assert!(s2.is_shutdown());
        token.cancelled().await;
    }

    #[tokio::test]
    async fn task_tracker_shutdown_aborts_tasks() {
        let mut tracker = TaskTracker::new();
        tracker.track(tokio::spawn(async {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        }));
        // Should complete quickly (abort + join)
        tracker.shutdown().await;
    }
}
