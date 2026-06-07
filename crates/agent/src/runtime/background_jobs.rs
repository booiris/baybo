//! Runtime side of the "Bash timeout → background" path: the
//! [`BackgroundJobSink`] the tool layer hands a still-running command to,
//! plus the detached escort that streams it to completion and routes a
//! [`AgentMessage::BackgroundJobFinished`] back to the parent session.
//!
//! It reuses the supervisor's in-flight-background registry (shared with
//! background subagents) for two things: pinning the parent against the
//! idle reaper while the command runs, and `/stop` suppression — `/stop`
//! cancels the registered token and drains the entry, so the escort kills
//! the child and skips delivery (a user-stopped result must not surface).
//! See `docs/todo/background-jobs.md`.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use aura_model::{PendingBackgroundResult, SessionId, SubagentExitStatus};
use aura_tools::{BackgroundJobSink, DetachedCommand};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::actor::AgentMessage;
use crate::actor::supervisor::AgentSupervisor;

/// In-flight-registry label for a detached command (the registry is shared
/// with background subagents, which use the subagent profile name here).
const COMMAND_JOB_LABEL: &str = "command";

/// How much of each output file to inline in the completion notification.
/// The full output stays on disk for the agent to `Read`.
const COMMAND_OUTPUT_TAIL_BYTES: u64 = 8 * 1024;

/// Implements [`BackgroundJobSink`]: takes a detached command, registers it
/// (pinning the parent), and spawns an escort that delivers the result when
/// it finishes. Constructed before the supervisor exists in the boot
/// sequence, so it holds the supervisor behind a `OnceLock` set right after
/// the supervisor is built.
pub struct BackgroundJobManager {
    supervisor: Arc<OnceLock<AgentSupervisor>>,
}

impl BackgroundJobManager {
    pub fn new(supervisor: Arc<OnceLock<AgentSupervisor>>) -> Self {
        Self { supervisor }
    }
}

#[async_trait]
impl BackgroundJobSink for BackgroundJobManager {
    async fn detach_command(&self, job: DetachedCommand) -> String {
        let handle_id = job.handle_id.clone();
        let parent_id = job.session_id.clone();
        // Synthetic child id = the handle, so the command rides the same
        // per-parent in-flight map background subagents use (pin + /stop).
        let registry_id = SessionId::from(handle_id.clone());
        let cancel = CancellationToken::new();
        if let Some(sup) = self.supervisor.get() {
            sup.note_background_subagent_started(
                &parent_id,
                &registry_id,
                COMMAND_JOB_LABEL,
                &job.command,
                cancel.clone(),
            );
        } else {
            warn!(
                %handle_id,
                "background command dispatched before supervisor wired; result will be lost"
            );
        }
        let supervisor = Arc::clone(&self.supervisor);
        let returned = handle_id.clone();
        tokio::spawn(async move {
            escort_command(supervisor, parent_id, registry_id, handle_id, cancel, job).await;
        });
        returned
    }
}

/// Await the detached child (or kill it on `/stop` / JobStop), flush its
/// output files, then route a completion notification to the parent —
/// unless `/stop` already drained the in-flight entry, in which case the
/// delivery is suppressed.
async fn escort_command(
    supervisor: Arc<OnceLock<AgentSupervisor>>,
    parent_id: SessionId,
    registry_id: SessionId,
    handle_id: String,
    cancel: CancellationToken,
    mut job: DetachedCommand,
) {
    let exit_code = tokio::select! {
        code = job.child.wait() => code,
        _ = cancel.cancelled() => {
            job.child.start_kill();
            let _ = job.child.wait().await;
            -1
        }
    };
    // Drain the stdout/stderr → file copy tasks so the files are fully
    // flushed before we read their tails.
    for task in job.copy_tasks.drain(..) {
        let _ = task.await;
    }

    let Some(sup) = supervisor.get() else {
        return;
    };
    // PEEK (don't clear) — an absent marker means `/stop` already drained
    // this command, so suppress delivery; the clear happens below.
    if sup.is_background_subagent_in_flight(&parent_id, &registry_id) {
        let tail = read_command_tail(&job.stdout_path, &job.stderr_path).await;
        let status = if cancel.is_cancelled() {
            SubagentExitStatus::Cancelled
        } else if exit_code == 0 {
            SubagentExitStatus::Completed
        } else {
            SubagentExitStatus::Failed {
                reason: format!("exit code {exit_code}"),
            }
        };
        let pending = PendingBackgroundResult::command(
            handle_id.clone(),
            job.command.clone(),
            exit_code,
            job.stdout_path.display().to_string(),
            tail,
            status,
        );
        let delivered = sup
            .route(
                &parent_id,
                AgentMessage::BackgroundJobFinished(Box::new(pending)),
            )
            .await;
        if !delivered {
            warn!(
                parent_session_id = %parent_id,
                %handle_id,
                "background command terminal could not be routed — parent actor not registered; output remains on disk"
            );
        }
    } else {
        debug!(
            parent_session_id = %parent_id,
            %handle_id,
            "background command was /stop-cancelled; suppressing delivery"
        );
    }
    sup.note_background_subagent_finished(&parent_id, &registry_id);
}

/// Combined tail of a command's stdout + stderr files for the completion
/// notification. Empty streams are dropped; if both have content the stderr
/// tail is fenced under a marker.
async fn read_command_tail(stdout_path: &Path, stderr_path: &Path) -> String {
    let out = read_file_tail(stdout_path, COMMAND_OUTPUT_TAIL_BYTES).await;
    let err = read_file_tail(stderr_path, COMMAND_OUTPUT_TAIL_BYTES).await;
    match (out.trim().is_empty(), err.trim().is_empty()) {
        (true, true) => "[no output]".to_string(),
        (false, true) => out,
        (true, false) => format!("[stderr]\n{err}"),
        (false, false) => format!("{out}\n[stderr]\n{err}"),
    }
}

/// Read the last `max_bytes` of a file (whole file if smaller), lossily as
/// UTF-8. Returns empty on any IO error (the file path is still surfaced).
async fn read_file_tail(path: &Path, max_bytes: u64) -> String {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
    let mut f = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let len = f.metadata().await.map(|m| m.len()).unwrap_or(0);
    if len > max_bytes {
        let _ = f.seek(SeekFrom::Start(len - max_bytes)).await;
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf).await;
    String::from_utf8_lossy(&buf).into_owned()
}
