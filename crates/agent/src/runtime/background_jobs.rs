//! Runtime side of background jobs. Two things live here:
//!
//! * [`background_eligible`] — the per-turn gate deciding whether work may be
//!   backgrounded at all. Both the `Bash` and the subagent path read it, so it
//!   is defined once here rather than in either consumer.
//! * The "Bash timeout → background" machinery: the [`BackgroundJobSink`] the
//!   tool layer hands a still-running command to, plus the detached escort
//!   that streams it to completion and routes a
//!   [`AgentMessage::BackgroundJobFinished`] back to the parent session.
//!
//! The escort reuses the supervisor's in-flight-background registry (shared
//! with background subagents) for two things: pinning the parent against the
//! idle reaper while the command runs, and `/stop` suppression — `/stop`
//! cancels the registered token and drains the entry, so the escort kills
//! the child and skips delivery (a user-stopped result must not surface).

use std::path::Path;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use baybo_model::{PendingBackgroundResult, Session, SessionId, SubagentExitStatus};
use baybo_tools::{BackgroundJobControl, BackgroundJobInfo, BackgroundJobSink, DetachedCommand};
use baybo_turn::TurnInputKind;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::actor::AgentMessage;
use crate::actor::supervisor::AgentSupervisor;

/// Whether this turn may **create** background work — a `Bash` command that
/// converts to background on timeout, or a subagent that converts (or is
/// dispatched) instead of blocking. Computed once per turn by the agent loop
/// and carried to both consumers as [`baybo_tools::ToolContext::background_eligible`].
/// Both halves of the gate:
///
/// * the **session** must be somewhere a completion notification can land
///   ([`Session::can_host_background_jobs`]);
/// * the **turn** must not be a cron fire's own turn. Converting there would
///   end the fire with a partial report and deliver the real answer as a
///   separate notification, so a fire blocks until its work is done.
///
/// A user reply inside a recurring fire's conversation is an ordinary
/// `UserChat` turn in a session that *can* host the notification, so it
/// backgrounds like any other chat.
pub(crate) fn background_eligible(session: &Session, turn_kind: TurnInputKind) -> bool {
    session.can_host_background_jobs() && !matches!(turn_kind, TurnInputKind::Cron)
}

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
    shutdown: CancellationToken,
}

impl BackgroundJobManager {
    pub fn new(supervisor: Arc<OnceLock<AgentSupervisor>>, shutdown: CancellationToken) -> Self {
        Self {
            supervisor,
            shutdown,
        }
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
                &handle_id,
                cancel.clone(),
            );
        } else {
            warn!(
                %handle_id,
                "background command dispatched before supervisor wired; result will be lost"
            );
        }
        let supervisor = Arc::clone(&self.supervisor);
        let shutdown = self.shutdown.clone();
        let returned = handle_id.clone();
        tokio::spawn(async move {
            escort_command(
                CommandEscort {
                    supervisor,
                    parent_id,
                    registry_id,
                    handle_id,
                    cancel,
                    shutdown,
                },
                job,
            )
            .await;
        });
        returned
    }
}

#[async_trait]
impl BackgroundJobControl for BackgroundJobManager {
    async fn list(&self, session_id: &SessionId) -> Vec<BackgroundJobInfo> {
        let Some(sup) = self.supervisor.get() else {
            return Vec::new();
        };
        sup.list_in_flight_background(session_id)
            .into_iter()
            // Report the advertised handle, not the registry key — for a
            // subagent the key is its child session id, but the agent only
            // knows the `bg-…` handle from the dispatch notice.
            .map(|(_, info)| BackgroundJobInfo {
                handle: info.handle,
                kind: info.kind,
                summary: info.task_summary,
            })
            .collect()
    }

    async fn stop(&self, session_id: &SessionId, handle: &str) -> bool {
        let Some(sup) = self.supervisor.get() else {
            return false;
        };
        sup.cancel_in_flight_background(session_id, handle)
            .is_some()
    }
}

/// Await the detached child (or kill it on `/stop` / JobStop), flush its
/// output files, then route a completion notification to the parent —
/// unless `/stop` already drained the in-flight entry, in which case the
/// delivery is suppressed.
struct CommandEscort {
    supervisor: Arc<OnceLock<AgentSupervisor>>,
    parent_id: SessionId,
    registry_id: SessionId,
    handle_id: String,
    cancel: CancellationToken,
    shutdown: CancellationToken,
}

async fn escort_command(escort: CommandEscort, mut job: DetachedCommand) {
    let CommandEscort {
        supervisor,
        parent_id,
        registry_id,
        handle_id,
        cancel,
        shutdown,
    } = escort;
    let outcome = tokio::select! {
        biased;
        _ = shutdown.cancelled() => {
            job.child.start_kill();
            let _ = job.child.wait().await;
            CommandOutcome::Shutdown
        }
        _ = cancel.cancelled() => {
            job.child.start_kill();
            let _ = job.child.wait().await;
            CommandOutcome::Stopped
        }
        code = job.child.wait() => CommandOutcome::Exited(code),
    };
    // Drain the stdout/stderr → file copy tasks so the files are fully
    // flushed before we read their tails.
    for task in job.copy_tasks.drain(..) {
        let _ = task.await;
    }

    if matches!(outcome, CommandOutcome::Shutdown) || shutdown.is_cancelled() {
        if let Some(sup) = supervisor.get() {
            sup.note_background_subagent_finished(&parent_id, &registry_id);
        }
        debug!(
            parent_session_id = %parent_id,
            %handle_id,
            "background command stopped during process shutdown; suppressing delivery"
        );
        return;
    }

    let Some(sup) = supervisor.get() else {
        return;
    };
    // PEEK (don't clear) — an absent marker means `/stop` already drained
    // this command, so suppress delivery; the clear happens below.
    if sup.is_background_subagent_in_flight(&parent_id, &registry_id) {
        let tail = read_command_tail(&job.stdout_path, &job.stderr_path).await;
        // No `Cancelled` arm: a `/stop` (or `JobStop`) cancels the token AND
        // drains the in-flight marker, so a cancelled command fails the peek
        // above and is suppressed — it never reaches this delivery branch.
        // Here the child ran to a real exit.
        let exit_code = match outcome {
            CommandOutcome::Exited(code) => code,
            CommandOutcome::Stopped => -1,
            CommandOutcome::Shutdown => return,
        };
        let status = if exit_code == 0 {
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

enum CommandOutcome {
    Exited(i32),
    Stopped,
    Shutdown,
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
#[cfg(test)]
mod background_eligible_tests {
    //! The turn half of the background-job gate. The session half lives on
    //! `Session::can_host_background_jobs` and is covered next to the
    //! spawner; what's pinned here is that the two compose so a recurring
    //! cron job's *fire* blocks while a *reply* in the same conversation
    //! backgrounds.
    use super::background_eligible;
    use baybo_model::{
        ChannelType, Lineage, LineageKind, Session, SessionId, SessionState, TriggerSource, TurnId,
        User,
    };
    use baybo_turn::TurnInputKind;
    use chrono::Utc;

    fn session_with(trigger: TriggerSource, lineage: Option<Lineage>) -> Session {
        let now = Utc::now();
        let id = SessionId::from("sess-bg");
        Session {
            id: id.clone(),
            user: User {
                id: "user-bg".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            channel: ChannelType::tui(),
            created_at: now,
            last_active: now,
            state: SessionState::default(),
            root_session_id: id,
            trigger,
            lineage,
            hidden: false,
            pinned: false,
            archived: false,
            folder_id: None,
            title: None,
        }
    }

    fn cron(conversation: bool) -> Session {
        session_with(
            TriggerSource::Cron {
                cron_job_id: "c-1".into(),
                origin_session_id: None,
                conversation,
                job_title: None,
                project_id: None,
            },
            None,
        )
    }

    #[test]
    fn a_user_chat_backgrounds() {
        let s = session_with(TriggerSource::User, None);
        assert!(background_eligible(&s, TurnInputKind::UserChat));
    }

    /// The point of the turn half: a fire that backgrounded its slow work
    /// would notify with a partial report and deliver the real answer
    /// separately, so it blocks even though its conversation could host the
    /// notification.
    #[test]
    fn a_recurring_fires_own_turn_blocks() {
        assert!(!background_eligible(&cron(true), TurnInputKind::Cron));
    }

    /// …and the same conversation backgrounds once the user takes over. This
    /// pair is the whole feature.
    #[test]
    fn a_reply_inside_a_recurring_fires_conversation_backgrounds() {
        assert!(background_eligible(&cron(true), TurnInputKind::UserChat));
    }

    /// A one-shot fire's workspace is invisible and unregistered, so nothing
    /// can be delivered there — no turn kind unlocks it.
    #[test]
    fn a_one_shot_fires_workspace_never_backgrounds() {
        assert!(!background_eligible(&cron(false), TurnInputKind::Cron));
        assert!(!background_eligible(&cron(false), TurnInputKind::UserChat));
    }

    /// A subagent's turn ends with the child, so there is no later turn to
    /// notify into regardless of what started it.
    #[test]
    fn a_subagent_session_never_backgrounds() {
        let s = session_with(
            TriggerSource::User,
            Some(Lineage {
                parent_session_id: SessionId::from("parent"),
                parent_turn_id: TurnId::new(),
                parent_span_id: None,
                kind: LineageKind::Subagent,
            }),
        );
        assert!(!background_eligible(&s, TurnInputKind::UserChat));
    }

    /// A background result's own notification turn must stay eligible: the
    /// agent reacting to one job may dispatch the next.
    #[test]
    fn a_notification_turn_still_backgrounds() {
        let s = session_with(TriggerSource::User, None);
        assert!(background_eligible(&s, TurnInputKind::SubagentNotification));
    }
}
