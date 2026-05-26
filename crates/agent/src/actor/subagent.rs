//! Agent-side wait routine the router runs after spawning the child
//! actor for a `SystemSpawnRequest::Subagent`. All protocol value
//! types live in `aura_model::spawn_protocol`; the tool that emits
//! the request lives in `aura_tools::builtin::spawn_subagent` and
//! sends directly on the same `mpsc::Sender<SystemSpawnRequest>` the
//! router consumes.

use std::sync::Arc;

use aura_channels::{AgentEvent, AgentOutput};
use aura_job::{JobLifecycle, JobStatusKind, JobTerminalEvent};
use aura_model::{ContentBlock, SessionId, SubagentExitStatus, SubagentResult};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::actor::AgentMessage;
use crate::actor::mailbox::MailboxSender;

/// Wait for a freshly-spawned subagent to terminate. The caller (router)
/// owns the synchronous prelude (build child session, spawn actor, send
/// initial message); this routine then watches output_rx for the final
/// message + terminal_rx for the job's terminal event, with a store
/// fallback on broadcast lag.
///
/// `actor_token` is the child actor's own
/// `VolatileResources::actor_token`, observed here so an external
/// cancel of the child surfaces as `Cancelled`.
pub async fn await_subagent_terminal(
    child_session_id: SessionId,
    mut output_rx: mpsc::Receiver<AgentOutput>,
    mut terminal_rx: broadcast::Receiver<JobTerminalEvent>,
    mailbox: MailboxSender<AgentMessage>,
    actor_token: CancellationToken,
    job_lifecycle: Arc<JobLifecycle>,
) -> SubagentResult {
    let mut captured: Option<Vec<ContentBlock>> = None;
    let wait_result = async {
        loop {
            tokio::select! {
                _ = actor_token.cancelled() => {
                    return Err(SubagentExitStatus::Cancelled);
                }
                msg = output_rx.recv() => {
                    match msg {
                        Some(AgentOutput {
                            event: AgentEvent::Message(m),
                            ..
                        }) => {
                            captured = Some(m.content);
                        }
                        Some(_) => continue,
                        None => return Err(SubagentExitStatus::Failed {
                            reason: "child output channel closed before terminal event".into(),
                        }),
                    }
                }
                event = terminal_rx.recv() => {
                    match event {
                        Ok(ev) if ev.session_id == child_session_id => {
                            if matches!(ev.kind, JobStatusKind::Completed) && captured.is_none() {
                                // `JobLifecycle::complete` publishes the terminal
                                // event inside `with_job` BEFORE
                                // `handle_subagent_spawned` dispatches the final
                                // `AgentEvent::Message`. Drain `output_rx` so the
                                // queued message isn't lost when the terminal
                                // event wins the select with `captured == None`.
                                captured = drain_for_final_message(&mut output_rx).await;
                            }
                            return terminal_event_to_status(ev.kind, captured.take());
                        }
                        Ok(_) => continue,
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!(
                                session_id = %child_session_id,
                                skipped = n,
                                "subagent waiter lagged on terminal-event bus, reconciling via store"
                            );
                            if let Some(kind) = check_child_terminal_via_store(
                                &job_lifecycle,
                                &child_session_id,
                            ).await {
                                if matches!(kind, JobStatusKind::Completed) && captured.is_none() {
                                    captured = drain_for_final_message(&mut output_rx).await;
                                }
                                return terminal_event_to_status(kind, captured.take());
                            }
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            return Err(SubagentExitStatus::Failed {
                                reason: "job lifecycle terminal-event bus closed".into(),
                            });
                        }
                    }
                }
            }
        }
    }
    .await;

    let _ = mailbox.send(AgentMessage::ActorStop).await;

    match wait_result {
        Ok(content) => SubagentResult {
            child_session_id,
            final_content: content,
            status: SubagentExitStatus::Completed,
        },
        Err(status) => SubagentResult {
            child_session_id,
            final_content: None,
            status,
        },
    }
}

fn terminal_event_to_status(
    kind: JobStatusKind,
    captured: Option<Vec<ContentBlock>>,
) -> Result<Option<Vec<ContentBlock>>, SubagentExitStatus> {
    match kind {
        JobStatusKind::Completed => Ok(captured),
        JobStatusKind::Failed => Err(SubagentExitStatus::Failed {
            reason: "child job failed".into(),
        }),
        JobStatusKind::Cancelled => Err(SubagentExitStatus::Cancelled),
        other => Err(SubagentExitStatus::Failed {
            reason: format!("unexpected non-terminal terminal-event kind: {other:?}"),
        }),
    }
}

/// Single-query store reconcile for the broadcast-lag path. Lists every
/// job on the child session, then picks the first terminal one — saves
/// the 3× round-trip of the per-status query the previous shape did.
async fn check_child_terminal_via_store(
    job_lifecycle: &JobLifecycle,
    child_session_id: &SessionId,
) -> Option<JobStatusKind> {
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
    jobs.into_iter().map(|j| j.status.kind()).find(|kind| {
        matches!(
            kind,
            JobStatusKind::Completed | JobStatusKind::Failed | JobStatusKind::Cancelled
        )
    })
}

/// Drain `output_rx` until the child actor's final `AgentEvent::Message`
/// arrives (or the channel closes). Other `AgentOutput` variants
/// (`Delta`, `Notice`) are skipped — only `Message` carries the
/// subagent's final content. Bounded by the caller's outer timeout.
async fn drain_for_final_message(
    output_rx: &mut mpsc::Receiver<AgentOutput>,
) -> Option<Vec<ContentBlock>> {
    while let Some(out) = output_rx.recv().await {
        if let AgentOutput {
            event: AgentEvent::Message(m),
            ..
        } = out
        {
            return Some(m.content);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::mailbox::{self, MailboxReceiver};
    use aura_channels::OutgoingMessage;
    use aura_job::test_support::MemoryJobStore;
    use aura_job::{JobInput, JobOutput};
    use aura_model::{ChannelType, MessageMetadata, TriggerKind};
    use std::time::Duration;
    use tokio::sync::mpsc;
    use tokio::task::JoinHandle;

    const CHILD_SESSION: &str = "child";

    struct Harness {
        job_lifecycle: Arc<JobLifecycle>,
        terminal_rx: tokio::sync::broadcast::Receiver<JobTerminalEvent>,
        output_tx: mpsc::Sender<AgentOutput>,
        output_rx: mpsc::Receiver<AgentOutput>,
        mailbox_tx: MailboxSender<AgentMessage>,
        _mailbox_rx: MailboxReceiver<AgentMessage>,
        actor_token: CancellationToken,
    }

    impl Harness {
        fn new() -> Self {
            let job_lifecycle = Arc::new(JobLifecycle::new(Arc::new(MemoryJobStore::new())));
            let terminal_rx = job_lifecycle.subscribe_terminal_events();
            let (output_tx, output_rx) = mpsc::channel(1);
            let (mailbox_tx, _mailbox_rx) = mailbox::channel(1);
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

        fn spawn_waiter(self) -> Waiter {
            Waiter {
                actor_token: self.actor_token.clone(),
                handle: tokio::spawn(await_subagent_terminal(
                    SessionId::from(CHILD_SESSION),
                    self.output_rx,
                    self.terminal_rx,
                    self.mailbox_tx,
                    self.actor_token,
                    self.job_lifecycle,
                )),
                output_tx: self.output_tx,
                _mailbox_rx: self._mailbox_rx,
            }
        }
    }

    /// Create + start an in-progress `Spawned` job on the child session.
    /// Mirrors what `with_job` does for a real subagent so tests can
    /// drive `JobLifecycle::complete` / observe `JobLifecycle::cancel`
    /// against a real row.
    async fn start_in_progress_child_job(lc: &JobLifecycle) -> aura_model::JobId {
        let job = lc
            .start_job(
                SessionId::from(CHILD_SESSION),
                TriggerKind::User,
                JobInput::Spawned {
                    initial_prompt: vec![ContentBlock::Text("task".into())],
                },
                "soul-v1",
                None,
            )
            .await
            .unwrap();
        lc.start(&job.id).await.unwrap();
        job.id
    }

    fn outgoing(text: &str) -> AgentOutput {
        OutgoingMessage {
            session_id: SessionId::from(CHILD_SESSION),
            user_id: String::new(),
            channel: ChannelType::from("subagent"),
            content: vec![ContentBlock::Text(text.into())],
            reply_to: None,
            metadata: MessageMetadata::default(),
            ordinal: None,
        }
        .into()
    }

    /// Spawned waiter plus the peer endpoints that must outlive it —
    /// dropping `output_tx` here would close `output_rx` mid-test and
    /// surface `Failed` regardless of intent. Tests that need the
    /// `JobLifecycle` after spawn clone it from the `Harness` before
    /// calling `spawn_waiter`.
    struct Waiter {
        actor_token: CancellationToken,
        handle: JoinHandle<SubagentResult>,
        output_tx: mpsc::Sender<AgentOutput>,
        _mailbox_rx: MailboxReceiver<AgentMessage>,
    }

    impl Waiter {
        async fn finish(self) -> SubagentResult {
            self.handle.await.unwrap()
        }
    }

    #[tokio::test]
    async fn external_cancel_surfaces_as_cancelled() {
        let w = Harness::new().spawn_waiter();
        w.actor_token.cancel();
        let result = w.finish().await;
        assert!(matches!(result.status, SubagentExitStatus::Cancelled));
    }

    #[tokio::test]
    async fn output_channel_close_surfaces_as_failed() {
        let mut h = Harness::new();
        h.close_output();
        let result = h.spawn_waiter().finish().await;
        assert!(matches!(result.status, SubagentExitStatus::Failed { .. }));
    }

    /// `JobLifecycle::complete` publishes the terminal event inside
    /// `with_job` BEFORE `handle_subagent_spawned` dispatches the final
    /// `AgentEvent::Message`. The waiter must keep draining `output_rx`
    /// after observing `Completed` with `captured == None`, otherwise
    /// the queued final Message is lost and the parent sees an empty
    /// "subagent completed without producing a final message" answer.
    #[tokio::test]
    async fn completed_event_drains_late_final_message() {
        let h = Harness::new();
        let lc = Arc::clone(&h.job_lifecycle);
        let job_id = start_in_progress_child_job(&lc).await;

        let w = h.spawn_waiter();

        // Publish terminal Completed BEFORE the final Message lands on
        // output_rx — the precise race the production path produces.
        lc.complete(
            &job_id,
            JobOutput::Message {
                content: vec![ContentBlock::Text("done".into())],
            },
        )
        .await
        .unwrap();
        // Hand the runtime a tick so the waiter pulls the terminal
        // event and enters drain_for_final_message before we send.
        tokio::time::sleep(Duration::from_millis(20)).await;
        w.output_tx.send(outgoing("hello")).await.unwrap();

        let result = w.finish().await;
        assert!(
            matches!(result.status, SubagentExitStatus::Completed),
            "expected Completed, got {:?}",
            result.status
        );
        let content = result
            .final_content
            .expect("drain must surface the late final Message");
        assert!(matches!(&content[..], [ContentBlock::Text(t)] if t == "hello"));
    }
}
