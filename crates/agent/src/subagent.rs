//! Agent-side wait routine the router runs after spawning the child
//! actor for a `SystemSpawnRequest::Subagent`. All protocol value
//! types live in `aura_model::spawn_protocol`; the tool that emits
//! the request lives in `aura_tools::builtin::spawn_subagent` and
//! sends directly on the same `mpsc::Sender<SystemSpawnRequest>` the
//! router consumes.

use std::sync::Arc;
use std::time::Duration;

use aura_channels::AgentOutput;
use aura_job::JobStatusKind;
use aura_model::{ContentBlock, SessionId, SubagentExitStatus, SubagentResult};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::actor::AgentMessage;
use crate::job::{JobLifecycle, JobTerminalEvent};

/// Wait for a freshly-spawned subagent to terminate. The caller (router)
/// owns the synchronous prelude (build child session, spawn actor, send
/// initial message); this routine then watches output_rx for the final
/// message + terminal_rx for the job's terminal event, with a store
/// fallback on broadcast lag.
///
/// `actor_token` must be the child actor's own
/// `VolatileResources::actor_token` — we cancel it on timeout, and a
/// sibling token here would leak the child's in-flight work.
pub async fn await_subagent_terminal(
    child_session_id: SessionId,
    mut output_rx: mpsc::Receiver<AgentOutput>,
    mut terminal_rx: broadcast::Receiver<JobTerminalEvent>,
    mailbox: mpsc::Sender<AgentMessage>,
    actor_token: CancellationToken,
    timeout: Duration,
    job_lifecycle: Arc<JobLifecycle>,
) -> SubagentResult {
    let mut captured: Option<Vec<ContentBlock>> = None;
    let wait_result = tokio::time::timeout(timeout, async {
        loop {
            tokio::select! {
                _ = actor_token.cancelled() => {
                    return Err(SubagentExitStatus::Cancelled);
                }
                msg = output_rx.recv() => {
                    match msg {
                        Some(AgentOutput::Message(m)) => {
                            captured = Some(m.content);
                        }
                        Some(_) => continue,
                        None => return Err(SubagentExitStatus::Failed(
                            "child output channel closed before terminal event".into(),
                        )),
                    }
                }
                event = terminal_rx.recv() => {
                    match event {
                        Ok(ev) if ev.session_id == child_session_id => {
                            return terminal_event_to_status(ev.kind, captured.take());
                        }
                        Ok(_) => continue,
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!(
                                session_id = %child_session_id,
                                skipped = n,
                                "subagent waiter lagged on terminal-event bus, reconciling via store"
                            );
                            if let Some(status) = check_child_terminal_via_store(
                                &job_lifecycle,
                                &child_session_id,
                                captured.take(),
                            ).await {
                                return status;
                            }
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            return Err(SubagentExitStatus::Failed(
                                "job lifecycle terminal-event bus closed".into(),
                            ));
                        }
                    }
                }
            }
        }
    })
    .await;

    let _ = mailbox.send(AgentMessage::Shutdown).await;

    match wait_result {
        Ok(Ok(content)) => SubagentResult {
            child_session_id,
            final_content: content,
            status: SubagentExitStatus::Completed,
        },
        Ok(Err(status)) => SubagentResult {
            child_session_id,
            final_content: None,
            status,
        },
        Err(_elapsed) => {
            actor_token.cancel();
            SubagentResult {
                child_session_id,
                final_content: None,
                status: SubagentExitStatus::Timeout,
            }
        }
    }
}

fn terminal_event_to_status(
    kind: JobStatusKind,
    captured: Option<Vec<ContentBlock>>,
) -> Result<Option<Vec<ContentBlock>>, SubagentExitStatus> {
    match kind {
        JobStatusKind::Completed => Ok(captured),
        JobStatusKind::Failed => Err(SubagentExitStatus::Failed("child job failed".into())),
        JobStatusKind::Cancelled => Err(SubagentExitStatus::Cancelled),
        other => Err(SubagentExitStatus::Failed(format!(
            "unexpected non-terminal terminal-event kind: {other:?}"
        ))),
    }
}

/// Single-query store reconcile for the broadcast-lag path. Lists every
/// job on the child session, then picks the first terminal one — saves
/// the 3× round-trip of the per-status query the previous shape did.
async fn check_child_terminal_via_store(
    job_lifecycle: &JobLifecycle,
    child_session_id: &SessionId,
    captured: Option<Vec<ContentBlock>>,
) -> Option<Result<Option<Vec<ContentBlock>>, SubagentExitStatus>> {
    let jobs = match job_lifecycle.list_by_session(child_session_id, None).await {
        Ok(j) => j,
        Err(e) => {
            warn!(
                session_id = %child_session_id,
                error = %e,
                "subagent reconcile via store failed; will keep waiting"
            );
            return None;
        }
    };
    for j in jobs {
        let kind = j.status.kind();
        if matches!(
            kind,
            JobStatusKind::Completed | JobStatusKind::Failed | JobStatusKind::Cancelled
        ) {
            return Some(terminal_event_to_status(kind, captured));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_storage::test_support::MemoryJobStore;
    use tokio::sync::mpsc;
    use tokio::task::JoinHandle;

    struct Harness {
        job_lifecycle: Arc<JobLifecycle>,
        terminal_rx: tokio::sync::broadcast::Receiver<JobTerminalEvent>,
        output_tx: mpsc::Sender<AgentOutput>,
        output_rx: mpsc::Receiver<AgentOutput>,
        mailbox_tx: mpsc::Sender<AgentMessage>,
        _mailbox_rx: mpsc::Receiver<AgentMessage>,
        actor_token: CancellationToken,
    }

    impl Harness {
        fn new() -> Self {
            let job_lifecycle = Arc::new(JobLifecycle::new(Arc::new(MemoryJobStore::new())));
            let terminal_rx = job_lifecycle.subscribe_terminal_events();
            let (output_tx, output_rx) = mpsc::channel(1);
            let (mailbox_tx, _mailbox_rx) = mpsc::channel(1);
            Self {
                job_lifecycle,
                terminal_rx,
                output_tx,
                output_rx,
                mailbox_tx,
                _mailbox_rx,
                actor_token: CancellationToken::new(),
            }
        }

        /// Drop the only sender so the waiter's `output_rx.recv()`
        /// resolves to `None` — mimics a child actor that exited
        /// before emitting a final Message.
        fn close_output(&mut self) {
            let (closed, _drop_rx) = mpsc::channel(1);
            self.output_tx = closed;
        }

        fn spawn_waiter(self, timeout: Duration) -> Waiter {
            Waiter {
                actor_token: self.actor_token.clone(),
                handle: tokio::spawn(await_subagent_terminal(
                    SessionId::from("child"),
                    self.output_rx,
                    self.terminal_rx,
                    self.mailbox_tx,
                    self.actor_token,
                    timeout,
                    self.job_lifecycle,
                )),
                _output_tx: self.output_tx,
                _mailbox_rx: self._mailbox_rx,
            }
        }
    }

    /// Spawned waiter plus the peer endpoints that must outlive it —
    /// dropping `output_tx` here would close `output_rx` mid-test and
    /// surface `Failed` regardless of intent.
    struct Waiter {
        actor_token: CancellationToken,
        handle: JoinHandle<SubagentResult>,
        _output_tx: mpsc::Sender<AgentOutput>,
        _mailbox_rx: mpsc::Receiver<AgentMessage>,
    }

    impl Waiter {
        async fn finish(self) -> SubagentResult {
            self.handle.await.unwrap()
        }
    }

    #[tokio::test]
    async fn timeout_cancels_passed_in_actor_token() {
        let w = Harness::new().spawn_waiter(Duration::from_millis(10));
        let actor_token = w.actor_token.clone();
        let result = w.finish().await;
        assert!(matches!(result.status, SubagentExitStatus::Timeout));
        assert!(
            actor_token.is_cancelled(),
            "actor_token must cancel so the child's in-flight work tears down"
        );
    }

    #[tokio::test]
    async fn external_cancel_surfaces_as_cancelled() {
        let w = Harness::new().spawn_waiter(Duration::from_secs(60));
        w.actor_token.cancel();
        let result = w.finish().await;
        assert!(matches!(result.status, SubagentExitStatus::Cancelled));
    }

    #[tokio::test]
    async fn output_channel_close_surfaces_as_failed() {
        let mut h = Harness::new();
        h.close_output();
        let result = h.spawn_waiter(Duration::from_secs(60)).finish().await;
        assert!(matches!(result.status, SubagentExitStatus::Failed(_)));
    }
}
