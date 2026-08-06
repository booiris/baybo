use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub struct McpRuntime {
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl McpRuntime {
    pub(crate) fn new(cancel: CancellationToken, task: JoinHandle<()>) -> Self {
        Self {
            cancel,
            task: Some(task),
        }
    }

    pub async fn shutdown(&mut self, deadline: tokio::time::Instant) {
        self.cancel.cancel();
        if let Some(mut task) = self.task.take() {
            match tokio::time::timeout_at(deadline, &mut task).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(%error, "mcp reconciler shutdown task failed"),
                Err(_) => {
                    tracing::warn!(
                        "mcp reconciler exceeded the runtime shutdown deadline; aborting"
                    );
                    task.abort();
                }
            }
        }
    }
}

impl Drop for McpRuntime {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_waits_for_cleanup() {
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let (cleaned_tx, cleaned_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            worker_cancel.cancelled().await;
            let _ = cleaned_tx.send(());
        });
        let mut runtime = McpRuntime::new(cancel, task);

        runtime
            .shutdown(tokio::time::Instant::now() + std::time::Duration::from_secs(1))
            .await;

        cleaned_rx.await.expect("cleanup task completed");
        assert!(runtime.task.is_none());
    }

    #[tokio::test]
    async fn shutdown_aborts_a_task_at_the_deadline() {
        let cancel = CancellationToken::new();
        let task = tokio::spawn(std::future::pending());
        let mut runtime = McpRuntime::new(cancel, task);

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            runtime.shutdown(tokio::time::Instant::now()),
        )
        .await
        .expect("shutdown returned after aborting the stuck task");

        assert!(runtime.task.is_none());
    }

    #[tokio::test]
    async fn drop_cancels_its_task() {
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            worker_cancel.cancelled().await;
            let _ = cancelled_tx.send(());
        });
        let runtime = McpRuntime::new(cancel, task);

        drop(runtime);

        tokio::time::timeout(std::time::Duration::from_secs(1), cancelled_rx)
            .await
            .expect("drop cancellation completed before timeout")
            .expect("drop cancellation task completed");
    }
}
