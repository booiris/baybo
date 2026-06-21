use std::collections::HashSet;
use std::time::Duration;

use aura_channels::{
    AgentEvent, AgentOutput, IncomingMessage, NoticeLevel, OutgoingMessage, STOP_COMMAND_NAME,
};
use aura_job::{CancelReason, JobStatusKind};
use aura_model::{ChannelType, ContentBlock, ControlEventKind, JobId, MessageMetadata, SessionId};
use tracing::{debug, warn};

use crate::actor::AgentMessage;
use crate::actor::supervisor::InFlightJob;

use super::Router;

/// How long `/stop` waits for a cancelled turn to fully unwind (persisting its
/// partial assistant row) before anchoring the stop control events. Generous —
/// the abort is near-instant — and a backstop so a wedged turn can't stall the
/// durable stop log indefinitely.
const STOP_SETTLE_TIMEOUT: Duration = Duration::from_secs(2);

impl Router {
    pub(super) async fn handle_incoming(
        &mut self,
        incoming: IncomingMessage,
    ) -> anyhow::Result<()> {
        // Reply target captured before `incoming` is consumed, so a rejection
        // inside `route_incoming` can still close the turn for the client.
        let session_id = SessionId::from(incoming.message.session_id.as_str());
        let user_id = incoming.message.sender.id.clone();
        let channel = incoming.message.channel.clone();
        let result = self.route_incoming(incoming).await;
        if let Err(e) = &result {
            // A pre-actor rejection (rate limit, cost cap, sanitizer, store
            // error, route failure) otherwise sends nothing back: a
            // request/response client like `aura prompt` then blocks for the
            // whole timeout, and a live channel just shows silence. Emit a
            // terminal `AgentEvent::Message` (via `From<OutgoingMessage>`) so
            // the turn closes with a visible reason.
            let reply = OutgoingMessage {
                session_id,
                user_id,
                channel,
                content: vec![ContentBlock::Text(format!("⚠️ {e}"))],
                reply_to: None,
                metadata: MessageMetadata::default(),
                ordinal: None,
            };
            self.handle_agent_output(reply.into()).await;
        }
        result
    }

    async fn route_incoming(&mut self, mut incoming: IncomingMessage) -> anyhow::Result<()> {
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
            // Carry the command text + send time so the persisted control events
            // record what the user did and when.
            self.handle_stop(
                &SessionId::from(session_id.as_str()),
                &user.id,
                &channel,
                &crate::actor::slash_command_text(&incoming.message.content),
                incoming.message.timestamp,
            )
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
        // session has no live actor yet. The spawner pins the loop to
        // `session.state.last_llm` — the per-session model the chat
        // `PUT /v1/chat/sessions/{id}/model` endpoint persisted — or
        // `None` (pool default) when the session was never switched. A
        // live actor is re-pinned in place via `AgentMessage::SetModel`,
        // so this read only matters for a cold spawn / post-eviction
        // hydration. (`SubagentSpawnRequest` is the other path that pins
        // a non-default model, via `model_tier`.)
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
                    let pinned = session.state.last_llm.clone();
                    actor_spawner(session, pinned, response_tx, actor_token)
                },
            )
            .await;
        if !routed {
            anyhow::bail!("failed to route user input to actor for session '{session_id}'");
        }

        Ok(())
    }

    /// Route a client batch ("send every queued message at once") as ONE
    /// coalesced turn. Mirrors [`Self::handle_incoming`]'s terminal-error
    /// reply so a pre-actor rejection still closes the turn for the client.
    pub(super) async fn handle_incoming_batch(
        &mut self,
        batch: Vec<IncomingMessage>,
    ) -> anyhow::Result<()> {
        let Some(first) = batch.first() else {
            return Ok(());
        };
        let session_id = SessionId::from(first.message.session_id.as_str());
        let user_id = first.message.sender.id.clone();
        let channel = first.message.channel.clone();
        let result = self.route_incoming_batch(batch).await;
        if let Err(e) = &result {
            let reply = OutgoingMessage {
                session_id,
                user_id,
                channel,
                content: vec![ContentBlock::Text(format!("⚠️ {e}"))],
                reply_to: None,
                metadata: MessageMetadata::default(),
                ordinal: None,
            };
            self.handle_agent_output(reply.into()).await;
        }
        result
    }

    /// Gate + sanitize a batch and route it as a single
    /// [`AgentMessage::UserInputBatch`]. The whole group is one user action, so
    /// rate-limit and cost gate once; each row is sanitized individually. The
    /// actor's `handle_merged_user_turn` then appends each as its own transcript
    /// row and answers the group with one reply. The batch is non-slash by
    /// construction — the client never batches a slash command (a coalescing
    /// barrier), sending those as individual messages instead.
    async fn route_incoming_batch(&mut self, batch: Vec<IncomingMessage>) -> anyhow::Result<()> {
        // Non-empty: handle_incoming_batch returned early otherwise. All entries
        // share one session — the client batches per session.
        let first = &batch[0];
        let session_id = first.message.session_id.clone();
        let user = first.message.sender.clone();
        let channel = first.message.channel.clone();
        let span = tracing::info_span!(
            "handle_incoming_batch",
            session_id = %session_id,
            count = batch.len(),
            channel = %channel,
        );
        let _guard = span.enter();

        if !self.rate_limiter.check(&user.id) {
            warn!(user_id = %user.id, session_id = %session_id, "user rate-limited (batch)");
            anyhow::bail!("rate limit exceeded for user '{}'", user.id);
        }
        self.cost_manager.check().map_err(|e| {
            warn!(user_id = %user.id, session_id = %session_id, error = %e, "cost manager rejected batch");
            anyhow::anyhow!(e)
        })?;

        let typed_session_id = SessionId::from(session_id.as_str());
        let mut session = self
            .session_manager
            .get_or_create(&typed_session_id, user, channel)
            .await?;

        // Sanitize each row independently. A message the security gateway
        // rejects is DROPPED (matching the single-message path's per-message
        // granularity) rather than failing the whole batch — one blocked
        // message must not silently discard the user's other valid ones.
        let mut sanitized = Vec::with_capacity(batch.len());
        for mut incoming in batch {
            if let Err(e) = self
                .security_gateway
                .sanitize_input(&mut incoming.message, &mut session)
                .await
            {
                warn!(
                    session_id = %session_id,
                    error = %e,
                    "security gateway blocked a batched message; dropping it"
                );
                continue;
            }
            sanitized.push(incoming);
        }
        if sanitized.is_empty() {
            return Ok(());
        }

        self.session_manager.touch(&typed_session_id).await?;

        // Defense-in-depth: a slash command is a coalescing barrier and must run
        // as its own turn (the web client never batches one, but a misbehaving
        // client could). If any survivor is a slash, route every message
        // INDIVIDUALLY so the actor's normal path applies slash semantics,
        // instead of merging a slash into `handle_merged_user_turn` as plain
        // text. The common all-non-slash case routes one atomic batch.
        if sanitized
            .iter()
            .any(|m| crate::actor::is_slash_command(&m.message.content))
        {
            for incoming in sanitized {
                if !self
                    .route_one_to_actor(
                        &session_id,
                        &session,
                        AgentMessage::UserInput(Box::new(incoming)),
                    )
                    .await
                {
                    anyhow::bail!(
                        "failed to route batched message to actor for session '{session_id}'"
                    );
                }
            }
            return Ok(());
        }

        if !self
            .route_one_to_actor(
                &session_id,
                &session,
                AgentMessage::UserInputBatch(sanitized),
            )
            .await
        {
            anyhow::bail!("failed to route user batch to actor for session '{session_id}'");
        }
        Ok(())
    }

    /// Route one `AgentMessage` to the session's actor, cold-spawning from
    /// `session` if needed. The spawn closure clones `session` so this can be
    /// called repeatedly (the slash-split batch path); the clone is only paid on
    /// the rare cold-spawn branch.
    async fn route_one_to_actor(
        &self,
        session_id: &SessionId,
        session: &aura_model::Session,
        message: AgentMessage,
    ) -> bool {
        let response_tx = self.supervisor.response_tx().clone();
        let parent_token = self.actor_parent_token.clone();
        let actor_spawner = self.actor_spawner.as_ref();
        let session = session.clone();
        self.supervisor
            .route_or_spawn(session_id, message, || {
                let actor_token = parent_token.child_token();
                let pinned = session.state.last_llm.clone();
                actor_spawner(session, pinned, response_tx, actor_token)
            })
            .await
    }

    /// Execute `/stop`: cancel the session's in-flight turn + every in-flight
    /// subagent it spawned, then acknowledge what was stopped. Results from
    /// subagents that already *completed* but haven't been reported yet are
    /// left untouched — `/stop` only stops what's still running, so those
    /// notify normally once the cancelled turn returns and the actor drains
    /// `pending_background_results`. Idempotent and safe on an idle session
    /// (everything degrades to a no-op).
    async fn handle_stop(
        &self,
        session_id: &SessionId,
        user_id: &str,
        channel: &ChannelType,
        command_text: &str,
        stopped_at: chrono::DateTime<chrono::Utc>,
    ) {
        // Drain the in-flight background subagents first: the removal both
        // gives us the cancel targets + ack summaries AND suppresses each
        // child's terminal delivery (its wait task sees the entry gone), so a
        // stopped result can't repopulate `pending_background_results`.
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
        // `list_active_turns_by_session` is store-filtered before applying the
        // turn-kind filter, so a long-lived session's full job history isn't
        // loaded just to find the live reply job(s).
        let mut cancelled_turn = false;
        let mut cancelled_turn_jobs: Vec<JobId> = Vec::new();
        match self
            .job_lifecycle
            .list_active_turns_by_session(session_id)
            .await
        {
            Ok(jobs) => {
                for job in jobs {
                    cancelled_turn = true;
                    cancelled_turn_jobs.push(job.id);
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
        // terminal). `list_active_turns_by_session` keeps the lookup bounded.
        for (child_session, info) in &background {
            if let Ok(jobs) = self
                .job_lifecycle
                .list_active_turns_by_session(child_session)
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

        // Note: we deliberately do NOT touch `pending_background_results` or
        // drain queued `BackgroundJobFinished`. Those hold results from subagents
        // that already finished; `/stop` stops running work, it doesn't discard
        // completed work — so once the cancelled turn returns, the actor reports
        // them via the normal notification path.
        let text = build_stop_notice(cancelled_turn, &background);

        // Fire the live notice first so the user sees the stop immediately,
        // independent of the durable-persist work below.
        self.handle_agent_output(AgentOutput {
            session_id: session_id.clone(),
            user_id: user_id.to_string(),
            channel: channel.clone(),
            event: AgentEvent::Notice {
                level: NoticeLevel::Info,
                text: text.clone(),
            },
        })
        .await;

        // Let each cancelled turn fully unwind before anchoring the control
        // events: the aborting loop persists its partial assistant row as it
        // tears down, so anchoring after the turn settles keeps the `/stop`
        // echo + notice after that row on reload (instead of racing it, which
        // would sort the cancelled turn's work block *after* the stop notice).
        for job_id in &cancelled_turn_jobs {
            self.job_lifecycle
                .wait_until_idle(job_id, STOP_SETTLE_TIMEOUT)
                .await;
        }

        // Record the user's `/stop` echo + the outcome notice as out-of-band
        // control events (separate from the LLM transcript) so a reload shows
        // both, anchored after the session's last row (the just-settled turn's
        // partial). A lookup error skips the persist (best-effort) rather than
        // mis-anchor it; the live notice above already fired.
        match self
            .session_manager
            .latest_session_ordinal(session_id)
            .await
        {
            Ok(max) => {
                let after = max.unwrap_or(-1);
                self.persist_control_event(
                    session_id,
                    after,
                    ControlEventKind::Command,
                    command_text,
                    stopped_at,
                )
                .await;
                self.persist_control_event(
                    session_id,
                    after,
                    ControlEventKind::NoticeInfo,
                    &text,
                    stopped_at,
                )
                .await;
            }
            Err(e) => warn!(
                %session_id,
                error = %e,
                "/stop: latest-ordinal lookup failed; skipping control-event persist"
            ),
        }
    }

    /// Append an out-of-band control event (slash-command echo / notice) to the
    /// session's control-event log. Best-effort — a write failure just means it
    /// won't reappear on reload.
    async fn persist_control_event(
        &self,
        session_id: &SessionId,
        after_ordinal: i64,
        kind: ControlEventKind,
        text: &str,
        at: chrono::DateTime<chrono::Utc>,
    ) {
        if let Err(e) = self
            .session_manager
            .append_control_event(session_id, after_ordinal, kind, text, at)
            .await
        {
            warn!(%session_id, error = %e, "failed to persist control event");
        }
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
fn build_stop_notice(cancelled_turn: bool, background: &[(SessionId, InFlightJob)]) -> String {
    if !cancelled_turn && background.is_empty() {
        return "Nothing in progress to stop.".to_string();
    }
    let mut lines = vec!["Stopped.".to_string()];
    if cancelled_turn {
        lines.push(aura_channels::STOP_CANCELLED_REPLY_LINE.to_string());
    }
    if !background.is_empty() {
        lines.push(format!(
            "- Cancelled {} background task(s):",
            background.len()
        ));
        for (_child, info) in background {
            lines.push(format!("  • [{}] {}", info.kind, info.task_summary));
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
                InFlightJob {
                    kind: "explorer".to_string(),
                    task_summary: "find X".to_string(),
                    handle: "bg-c1".to_string(),
                    cancel_token: CancellationToken::new(),
                },
            ),
            (
                SessionId::from("c2"),
                InFlightJob {
                    kind: "planner".to_string(),
                    task_summary: "draft Y".to_string(),
                    handle: "bg-c2".to_string(),
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
