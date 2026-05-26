use std::collections::HashSet;

use aura_channels::{AgentEvent, AgentOutput, IncomingMessage, NoticeLevel, STOP_COMMAND_NAME};
use aura_job::{CancelReason, JobStatusKind};
use aura_model::{ChannelType, ContentBlock, JobId, SessionId};
use tracing::{debug, warn};

use crate::actor::AgentMessage;
use crate::actor::supervisor::InFlightSubagent;

use super::Router;

impl Router {
    pub(super) async fn handle_incoming(
        &mut self,
        mut incoming: IncomingMessage,
    ) -> anyhow::Result<()> {
        let span = tracing::info_span!(
            "handle_incoming",
            session_id = %incoming.message.session_id,
            message_id = %incoming.message.id,
            channel = %incoming.message.channel,
        );
        let _guard = span.enter();

        let session_id = incoming.message.session_id.clone();
        let user = incoming.message.sender.clone();
        let channel = incoming.message.channel.clone();

        // `/stop` is an out-of-band control command: cancel the session's
        // in-flight turn + subagents and clear queued notifications WITHOUT
        // routing a turn. Handled in the Router (not the actor) because a busy
        // actor isn't reading its mailbox and so can't preempt its own running
        // turn. Recognised before the rate-limit / cost gate so a stop always
        // lands. No `get_or_create`: nothing to stop on a session with no actor.
        if is_stop_command(&incoming.message.content) {
            self.handle_stop(&SessionId::from(session_id.as_str()), &user.id, &channel)
                .await;
            return Ok(());
        }

        debug!(session_id = %session_id, user_id = %user.id, "routing message");

        // User-level rate limiting
        if !self.rate_limiter.check(&user.id) {
            warn!(
                user_id = %user.id,
                session_id = %session_id,
                "user rate-limited"
            );
            anyhow::bail!("rate limit exceeded for user '{}'", user.id);
        }

        // In-memory budget gate: same call agent_loop makes before
        // each LLM call, fired here too so an over-cap user never even
        // gets an actor spun up.
        self.cost_manager.check().map_err(|e| {
            warn!(
                user_id = %user.id,
                session_id = %session_id,
                error = %e,
                "cost manager rejected request"
            );
            anyhow::anyhow!(e)
        })?;

        // Get or create session
        let typed_session_id = SessionId::from(session_id.as_str());
        let mut session = self
            .session_manager
            .get_or_create(&typed_session_id, user, channel)
            .await?;

        // Sanitize input through the security gateway before routing.
        if let Err(e) = self
            .security_gateway
            .sanitize_input(&mut incoming.message, &mut session)
            .await
        {
            warn!(
                session_id = %session_id,
                error = %e,
                "security gateway blocked or modified incoming message"
            );
            return Err(e.into());
        }

        // Update last active time
        self.session_manager.touch(&typed_session_id).await?;

        // Route to the session's actor, lazily spawning one if the
        // session has no live actor yet. User-channel actors are not
        // allowed to override the LLM at spawn time: the spawner reads
        // `session.state.last_llm` (set by admin-side session creation)
        // for hydration, or falls back to the pool default. Mid-session
        // swaps are not exposed to user-channel callers —
        // `SubagentSpawnRequest` is the only path that pins a
        // non-default model.
        let response_tx = self.supervisor.response_tx().clone();
        let parent_token = self.actor_parent_token.clone();
        let actor_spawner = self.actor_spawner.as_ref();
        let routed = self
            .supervisor
            .route_or_spawn(
                &session_id,
                AgentMessage::UserInput(Box::new(incoming)),
                || {
                    let actor_token = parent_token.child_token();
                    actor_spawner(session, None, response_tx, actor_token)
                },
            )
            .await;
        if !routed {
            warn!(session_id = %session_id, "failed to route user input to actor");
        }

        Ok(())
    }

    /// Execute `/stop`: cancel the session's in-flight turn + every in-flight
    /// subagent it spawned, then acknowledge what was stopped. Results from
    /// subagents that already *completed* but haven't been reported yet are
    /// left untouched — `/stop` only stops what's still running, so those
    /// notify normally once the cancelled turn returns and the actor drains
    /// `pending_subagent_results`. Idempotent and safe on an idle session
    /// (everything degrades to a no-op).
    async fn handle_stop(&self, session_id: &SessionId, user_id: &str, channel: &ChannelType) {
        // Drain the in-flight background subagents first: the removal both
        // gives us the cancel targets + ack summaries AND suppresses each
        // child's terminal delivery (its wait task sees the entry gone), so a
        // stopped result can't repopulate `pending_subagent_results`.
        let background = self
            .supervisor
            .take_in_flight_background_subagents(session_id);

        // Cancel the in-flight turn, then walk its in-flight descendants.
        // Cancelling the turn job trips the turn's loop cancel token first,
        // which cascades into any foreground subagents (their tokens descend
        // from it) and aborts the turn's own await immediately — so the
        // descendant walk is a best-effort `UserStopped` audit stamp + backstop,
        // not the load-bearing stop (a foreground child cancelled via cascade
        // ends up `ParentCancelled`, which is the accurate reason for it).
        // `list_active_by_session` is store-filtered, so a long-lived session's
        // full job history isn't loaded just to find the live few.
        let mut cancelled_turn = false;
        match self.job_lifecycle.list_active_by_session(session_id).await {
            Ok(jobs) => {
                for job in jobs {
                    cancelled_turn = true;
                    let _ = self
                        .job_lifecycle
                        .cancel(&job.id, CancelReason::UserStopped, vec![])
                        .await;
                    self.cancel_in_flight_descendants(&job.id).await;
                }
            }
            Err(e) => warn!(session_id = %session_id, error = %e, "stop: list session jobs failed"),
        }

        // Stop each drained background subagent. Cancel its running job with
        // `UserStopped` FIRST — that trips the loop token AND makes the reason
        // win the race against the escort's `ParentCancelled` — then trip the
        // stored token to also cover the pre-job-dispatch window: when no row
        // exists yet the token is the only handle, and the child aborts at
        // iteration 0 once it spawns its job (`with_job` then flips that row
        // terminal). `list_active_by_session` keeps the lookup bounded.
        for (child_session, info) in &background {
            if let Ok(jobs) = self
                .job_lifecycle
                .list_active_by_session(child_session)
                .await
            {
                for job in jobs {
                    let _ = self
                        .job_lifecycle
                        .cancel(&job.id, CancelReason::UserStopped, vec![])
                        .await;
                }
            }
            info.cancel_token.cancel();
        }

        // Note: we deliberately do NOT touch `pending_subagent_results` or
        // drain queued `SubagentFinished`. Those hold results from subagents
        // that already finished; `/stop` stops running work, it doesn't discard
        // completed work — so once the cancelled turn returns, the actor reports
        // them via the normal notification path.
        self.handle_agent_output(AgentOutput {
            session_id: session_id.clone(),
            user_id: user_id.to_string(),
            channel: channel.clone(),
            event: AgentEvent::Notice {
                level: NoticeLevel::Info,
                text: build_stop_notice(cancelled_turn, &background),
            },
        })
        .await;
    }

    /// Cancel every in-flight job in the subtree rooted at `root_job_id`
    /// (foreground subagents, at any depth). Iterative BFS so a deep lineage
    /// doesn't need boxed async recursion.
    async fn cancel_in_flight_descendants(&self, root_job_id: &JobId) {
        let mut visited = HashSet::new();
        let mut worklist = vec![*root_job_id];
        while let Some(job_id) = worklist.pop() {
            // `parent_job_id` is immutable so the lineage is a tree today, but
            // a visited-set keeps this from spinning forever if a future
            // re-parenting feature ever introduces a cycle.
            if !visited.insert(job_id) {
                continue;
            }
            match self.job_lifecycle.list_children(&job_id).await {
                Ok(children) => {
                    for child in children
                        .into_iter()
                        .filter(|c| is_in_flight(c.status.kind()))
                    {
                        let _ = self
                            .job_lifecycle
                            .cancel(&child.id, CancelReason::UserStopped, vec![])
                            .await;
                        worklist.push(child.id);
                    }
                }
                Err(e) => warn!(job_id = %job_id, error = %e, "stop: list child jobs failed"),
            }
        }
    }
}

/// True for the non-terminal job states `/stop` should cancel.
fn is_in_flight(kind: JobStatusKind) -> bool {
    matches!(
        kind,
        JobStatusKind::Pending | JobStatusKind::InProgress | JobStatusKind::Stuck
    )
}

/// Recognise `/stop` — first text block, trimmed, leading `/`, first token
/// matching `STOP_COMMAND_NAME` (case-insensitive, trailing args ignored).
/// Mirrors the gateway/TUI slash shape so the command is consistent.
fn is_stop_command(content: &[ContentBlock]) -> bool {
    let Some(ContentBlock::Text(text)) = content.first() else {
        return false;
    };
    let Some(rest) = text.trim().strip_prefix('/') else {
        return false;
    };
    let token = rest.split_whitespace().next().unwrap_or("");
    // Telegram/Discord group commands arrive as `/stop@BotName` so one bot in
    // the group picks them up; strip the optional `@<bot>` suffix before
    // matching, mirroring the gateway slash parser (`/stop` is PassThrough, so
    // the un-stripped token reaches here).
    token
        .split('@')
        .next()
        .unwrap_or("")
        .eq_ignore_ascii_case(STOP_COMMAND_NAME)
}

/// Compose the `/stop` acknowledgement: confirm what was cancelled and list
/// each background task by its type + summary so the user sees exactly what
/// was dropped. Idle session → a plain "nothing to stop".
fn build_stop_notice(cancelled_turn: bool, background: &[(SessionId, InFlightSubagent)]) -> String {
    if !cancelled_turn && background.is_empty() {
        return "Nothing in progress to stop.".to_string();
    }
    let mut lines = vec!["Stopped.".to_string()];
    if cancelled_turn {
        lines.push("- Cancelled the in-progress reply.".to_string());
    }
    if !background.is_empty() {
        lines.push(format!(
            "- Cancelled {} background task(s):",
            background.len()
        ));
        for (_child, info) in background {
            lines.push(format!(
                "  • [{}] {}",
                info.subagent_type, info.task_summary
            ));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn text(s: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text(s.to_string())]
    }

    #[test]
    fn recognises_stop_command_case_args_and_bot_suffix() {
        assert!(is_stop_command(&text("/stop")));
        assert!(is_stop_command(&text("  /Stop  ")));
        assert!(is_stop_command(&text("/STOP everything now")));
        // Telegram/Discord group form `/stop@BotName` (suffix stripped).
        assert!(is_stop_command(&text("/stop@MyBot")));
        assert!(is_stop_command(&text("/Stop@MyBot please")));
        // Not a stop command.
        assert!(!is_stop_command(&text("stop")));
        assert!(!is_stop_command(&text("/stopwatch")));
        assert!(!is_stop_command(&text("/stopwatch@MyBot")));
        assert!(!is_stop_command(&text("/compact")));
        assert!(!is_stop_command(&text("please /stop")));
        assert!(!is_stop_command(&[]));
    }

    #[test]
    fn stop_notice_idle_vs_active() {
        // Idle: nothing cancelled.
        assert_eq!(
            build_stop_notice(false, &[]),
            "Nothing in progress to stop."
        );
        // Turn only.
        assert_eq!(
            build_stop_notice(true, &[]),
            "Stopped.\n- Cancelled the in-progress reply."
        );
        // Background tasks listed by type + summary.
        let bg = vec![
            (
                SessionId::from("c1"),
                InFlightSubagent {
                    subagent_type: "explorer".to_string(),
                    task_summary: "find X".to_string(),
                    cancel_token: CancellationToken::new(),
                },
            ),
            (
                SessionId::from("c2"),
                InFlightSubagent {
                    subagent_type: "planner".to_string(),
                    task_summary: "draft Y".to_string(),
                    cancel_token: CancellationToken::new(),
                },
            ),
        ];
        let notice = build_stop_notice(true, &bg);
        assert!(notice.starts_with("Stopped.\n- Cancelled the in-progress reply."));
        assert!(notice.contains("- Cancelled 2 background task(s):"));
        assert!(notice.contains("  • [explorer] find X"));
        assert!(notice.contains("  • [planner] draft Y"));
        // Background without a turn omits the reply line.
        assert!(!build_stop_notice(false, &bg).contains("in-progress reply"));
    }

    #[test]
    fn in_flight_covers_non_terminal_states_only() {
        assert!(is_in_flight(JobStatusKind::Pending));
        assert!(is_in_flight(JobStatusKind::InProgress));
        assert!(is_in_flight(JobStatusKind::Stuck));
        assert!(!is_in_flight(JobStatusKind::Completed));
        assert!(!is_in_flight(JobStatusKind::Failed));
        assert!(!is_in_flight(JobStatusKind::Cancelled));
    }
}
