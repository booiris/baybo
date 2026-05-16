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
/// fallback on broadcast lag. On timeout the child token is tripped so
/// the child's descendants cascade-cancel.
pub async fn await_subagent_terminal(
    child_session_id: SessionId,
    mut output_rx: mpsc::Receiver<AgentOutput>,
    mut terminal_rx: broadcast::Receiver<JobTerminalEvent>,
    mailbox: mpsc::Sender<AgentMessage>,
    parent_token: CancellationToken,
    timeout: Duration,
    job_lifecycle: Arc<JobLifecycle>,
) -> SubagentResult {
    let child_token = parent_token.child_token();
    let mut captured: Option<Vec<ContentBlock>> = None;
    let wait_result = tokio::time::timeout(timeout, async {
        loop {
            tokio::select! {
                _ = child_token.cancelled() => {
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
            child_token.cancel();
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
