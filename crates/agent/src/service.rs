use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tokio::sync::Notify;
use tracing::info;

/// Coordinates graceful shutdown across all components.
#[derive(Clone)]
pub struct ShutdownSignal {
    shutdown: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl ShutdownSignal {
    pub fn new() -> Self {
        Self {
            shutdown: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Check if shutdown has been requested.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Trigger shutdown — wakes all waiters.
    pub fn trigger(&self) {
        info!("shutdown triggered");
        self.shutdown.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Wait until shutdown is triggered.
    pub async fn wait(&self) {
        if self.is_shutdown() {
            return;
        }
        self.notify.notified().await;
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
        s1.trigger();
        assert!(s2.is_shutdown());
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
