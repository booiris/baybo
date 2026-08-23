use std::collections::HashSet;
use std::time::Duration;

use baybo_channels::wire::MAX_MESSAGE_BATCH_MESSAGES;
use baybo_channels::{
    AgentEvent, AgentOutput, IncomingMessage, NoticeLevel, OutgoingMessage, STOP_COMMAND_NAME,
};
use baybo_model::{
    ChannelType, ContentBlock, ControlEventKind, MessageMetadata, SessionId, TurnId,
};
use baybo_turn::{CancelReason, TurnStatusKind};
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
        let bot_id = incoming.bot_id.clone();
        let platform_msg_id = incoming.platform_msg_id.clone();
        let result = self.route_incoming(incoming).await;
        if let Err(e) = &result {
            // The channel layer recorded the dedup key BEFORE these gates ran
            // (it must — the echo follows the record), and a rejected message
            // persists nothing. Un-record it, or every retransmission of the
            // same `platform_msg_id` is silently swallowed and the send is
            // permanently unsendable — the client outbox retries under the
            // same id by design. `/stop` never reaches here (handled, Ok).
            self.inbound_dedup
                .remove(&channel, &bot_id, &platform_msg_id);
            // A pre-actor rejection (rate limit, cost cap, sanitizer, store
            // error, route failure) otherwise sends nothing back: a
            // request/response client like `baybo prompt` then blocks for the
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
                &incoming.platform_msg_id,
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
        let message = AgentMessage::UserInput(Box::new(incoming));
        // A live actor already carries its pins, and resolving them reads the
        // profile row — a query per message on a session that needs none of
        // it. `route_or_spawn`'s closure is synchronous, so the only way to
        // skip that read is to find out first whether a spawn is coming.
        let routed = match self
            .supervisor
            .route_existing(&session_id, message.clone())
            .await
        {
            Some(routed) => routed,
            None => {
                let pins = super::resolve_spawn_pins(&session, &self.agent_profiles).await;
                self.supervisor
                    .route_or_spawn(&session_id, message, || {
                        let actor_token = parent_token.child_token();
                        actor_spawner(
                            session,
                            pins.entry,
                            pins.model,
                            pins.effort,
                            response_tx,
                            actor_token,
                        )
                    })
                    .await
            }
        };
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
        let dedup_keys: Vec<(ChannelType, String, String)> = batch
            .iter()
            .map(|m| {
                (
                    m.message.channel.clone(),
                    m.bot_id.clone(),
                    m.platform_msg_id.clone(),
                )
            })
            .collect();
        let result = self.route_incoming_batch(batch).await;
        if let Err(e) = &result {
            // Same un-record as `handle_incoming`, for every row: a batch
            // bail (size cap, rate limit, cost, store, route) rejected the
            // whole group pre-persist. Two accepted imprecisions. (1) The
            // slash-split route failure, where earlier rows may already have
            // routed — `route_or_spawn` only fails while the supervisor is
            // tearing down, and a re-admitted duplicate cannot run on a
            // process that is exiting. (2) A sanitize-dropped row was already
            // un-recorded in `route_incoming_batch`, and this second remove is
            // NOT strictly idempotent: if a retry of that row re-recorded the
            // key in the microseconds between drop and bail, this deletes the
            // retry's live record, and a further duplicate could double-route.
            // The client's retries run on 10s+ timers against a ms-scale
            // window, and the row in question was security-rejected — the
            // retry will be rejected again — so the compound race is accepted
            // over threading a per-row drop set out of the routing path.
            for (ch, bot, id) in &dedup_keys {
                self.inbound_dedup.remove(ch, bot, id);
            }
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
        if batch.len() > MAX_MESSAGE_BATCH_MESSAGES {
            anyhow::bail!("message batch exceeds {MAX_MESSAGE_BATCH_MESSAGES} messages");
        }
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

        for _ in 0..batch.len() {
            if !self.rate_limiter.check(&user.id) {
                warn!(user_id = %user.id, session_id = %session_id, "user rate-limited (batch)");
                anyhow::bail!("rate limit exceeded for user '{}'", user.id);
            }
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
                // Dropped with an Ok result — the batch choke point never
                // sees this row, so its dedup key must be un-recorded here
                // or it burns (see `handle_incoming`).
                self.inbound_dedup.remove(
                    &incoming.message.channel,
                    &incoming.bot_id,
                    &incoming.platform_msg_id,
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
        session: &baybo_model::Session,
        message: AgentMessage,
    ) -> bool {
        let response_tx = self.supervisor.response_tx().clone();
        let parent_token = self.actor_parent_token.clone();
        let actor_spawner = self.actor_spawner.as_ref();
        let session = session.clone();
        let pins = super::resolve_spawn_pins(&session, &self.agent_profiles).await;
        self.supervisor
            .route_or_spawn(session_id, message, || {
                let actor_token = parent_token.child_token();
                actor_spawner(
                    session,
                    pins.entry,
                    pins.model,
                    pins.effort,
                    response_tx,
                    actor_token,
                )
            })
            .await
    }

    /// Execute `/stop`: cancel the session's in-flight turn + every in-flight
    /// subagent it spawned, then acknowledge what was stopped. Results from
    /// subagents that already *completed* but haven't been reported yet are
    /// left untouched — `/stop` only stops what's still running, so those
    /// notify normally once the cancelled turn returns and the actor drains
    /// its background-notification buffer. Idempotent and safe on an idle
    /// session (everything degrades to a no-op).
    async fn handle_stop(
        &self,
        session_id: &SessionId,
        user_id: &str,
        channel: &ChannelType,
        command_text: &str,
        stopped_at: chrono::DateTime<chrono::Utc>,
        command_msg_id: &str,
    ) {
        // Drain the in-flight background subagents first: the removal both
        // gives us the cancel targets + ack summaries AND suppresses each
        // child's terminal delivery (its wait task sees the entry gone), so a
        // stopped result can't repopulate the notification buffer.
        let background = self
            .supervisor
            .take_in_flight_background_subagents(session_id);

        // Cancel the in-flight turn, then walk its in-flight descendants.
        // Cancelling the turn trips the turn's loop cancel token first,
        // which cascades into any foreground subagents (their tokens descend
        // from it) and aborts the turn's own await immediately — so the
        // descendant walk is a best-effort `UserStopped` audit stamp + backstop,
        // not the load-bearing stop (a foreground child cancelled via cascade
        // ends up `ParentCancelled`, which is the accurate reason for it).
        // `list_active_chat_turns_by_session` is store-filtered before applying the
        // turn-kind filter, so a long-lived session's full turn history isn't
        // loaded just to find the live reply turn(s).
        let mut cancelled_turn = false;
        let mut cancelled_turn_turns: Vec<TurnId> = Vec::new();
        match self
            .turn_lifecycle
            .list_active_chat_turns_by_session(session_id)
            .await
        {
            Ok(turns) => {
                for turn in turns {
                    cancelled_turn = true;
                    cancelled_turn_turns.push(turn.id);
                    let _ = self
                        .turn_lifecycle
                        .cancel(&turn.id, CancelReason::UserStopped, vec![])
                        .await;
                    self.cancel_in_flight_descendants(&turn.id).await;
                }
            }
            Err(e) => {
                warn!(session_id = %session_id, error = %e, "stop: list session turns failed")
            }
        }

        // Stop each drained background subagent. Cancel its running turn with
        // `UserStopped` FIRST — that trips the loop token AND makes the reason
        // win the race against the escort's `ParentCancelled` — then trip the
        // stored token to also cover the pre-turn-dispatch window: when no row
        // exists yet the token is the only handle, and the child aborts at
        // iteration 0 once it spawns its turn (`with_turn` then flips that row
        // terminal). `list_active_chat_turns_by_session` keeps the lookup bounded.
        for (child_session, info) in &background {
            if let Ok(turns) = self
                .turn_lifecycle
                .list_active_chat_turns_by_session(child_session)
                .await
            {
                for turn in turns {
                    let _ = self
                        .turn_lifecycle
                        .cancel(&turn.id, CancelReason::UserStopped, vec![])
                        .await;
                }
            }
            info.cancel_token.cancel();
        }

        // Note: we deliberately do not touch durable background-notification
        // state or drain queued `BackgroundJobFinished` messages. Those hold
        // already-completed work, which reports after the cancelled turn returns.
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
                mid_turn: false,
                // Persisted only AFTER this emit (the id is unknown here);
                // clients pull the durable row via sync instead.
                durable_id: None,
            },
        })
        .await;

        // Let each cancelled turn fully unwind before anchoring the control
        // events: the aborting loop persists its partial assistant row as it
        // tears down, so anchoring after the turn settles keeps the `/stop`
        // echo + notice after that row on reload (instead of racing it, which
        // would sort the cancelled turn's work block *after* the stop notice).
        for turn_id in &cancelled_turn_turns {
            self.turn_lifecycle
                .wait_until_idle(turn_id, STOP_SETTLE_TIMEOUT)
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
                // The Command echo carries the send's id so a client's
                // optimistic `/stop` bubble reconciles with it; the ack notice
                // is server-authored, so it has none.
                self.persist_control_event(
                    session_id,
                    after,
                    ControlEventKind::Command,
                    command_text,
                    stopped_at,
                    command_msg_id,
                )
                .await;
                self.persist_control_event(
                    session_id,
                    after,
                    ControlEventKind::NoticeInfo,
                    &text,
                    stopped_at,
                    "",
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
        platform_msg_id: &str,
    ) {
        if let Err(e) = self
            .session_manager
            .append_control_event(session_id, after_ordinal, kind, text, at, platform_msg_id)
            .await
        {
            warn!(%session_id, error = %e, "failed to persist control event");
        }
    }

    /// Cancel every in-flight turn in the subtree rooted at `root_turn_id`
    /// (foreground subagents, at any depth). Iterative BFS so a deep lineage
    /// doesn't need boxed async recursion.
    async fn cancel_in_flight_descendants(&self, root_turn_id: &TurnId) {
        let mut visited = HashSet::new();
        let mut worklist = vec![*root_turn_id];
        while let Some(turn_id) = worklist.pop() {
            // `parent_turn_id` is immutable so the lineage is a tree today, but
            // a visited-set keeps this from spinning forever if a future
            // re-parenting feature ever introduces a cycle.
            if !visited.insert(turn_id) {
                continue;
            }
            match self.turn_lifecycle.list_children(&turn_id).await {
                Ok(children) => {
                    for child in children
                        .into_iter()
                        .filter(|c| is_in_flight(c.status.kind()))
                    {
                        let _ = self
                            .turn_lifecycle
                            .cancel(&child.id, CancelReason::UserStopped, vec![])
                            .await;
                        worklist.push(child.id);
                    }
                }
                Err(e) => warn!(turn_id = %turn_id, error = %e, "stop: list child turns failed"),
            }
        }
    }
}

/// True for the non-terminal turn states `/stop` should cancel.
fn is_in_flight(kind: TurnStatusKind) -> bool {
    matches!(
        kind,
        TurnStatusKind::Pending | TurnStatusKind::InProgress | TurnStatusKind::Stuck
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
        lines.push(baybo_channels::STOP_CANCELLED_REPLY_LINE.to_string());
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
    use std::collections::HashMap;
    use std::sync::Arc;

    use baybo_channels::{ChannelRegistry, InboundDedup, Message};
    use baybo_cost::test_support::MemoryCostStore;
    use baybo_cost::{CostManager, SpendingLimits};
    use baybo_cron::CronStore;
    use baybo_cron::test_support::InMemoryCronStore;
    use baybo_model::User;
    use baybo_security::test_support::MemorySecretStore;
    use baybo_security::{EncryptionKey, LeakAction, LeakDetectionRule, LeakDetector, SecretVault};
    use baybo_session::SessionManager;
    use baybo_session::test_support::{MemorySessionFolderStore, MemorySessionStore};
    use baybo_turn::TurnLifecycle;
    use baybo_turn::test_support::MemoryTurnStore;
    use chrono::Utc;
    use parking_lot::Mutex;
    use regex::Regex;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use crate::actor::mailbox::{self, MailboxReceiver};
    use crate::actor::router::{ActorSpawner, LiveRateLimit, RouterConfig};
    use crate::actor::supervisor::AgentSupervisor;
    use crate::security::SecurityGateway;

    fn text(s: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text(s.to_string())]
    }

    fn incoming(platform_msg_id: &str) -> IncomingMessage {
        IncomingMessage {
            message: Message {
                id: format!("id-{platform_msg_id}"),
                session_id: SessionId::from("sess-1"),
                channel: ChannelType::tui(),
                sender: User {
                    id: "u1".into(),
                    name: None,
                    channel: ChannelType::tui(),
                },
                content: text("hello"),
                timestamp: Utc::now(),
                reply_to: None,
                metadata: MessageMetadata::default(),
            },
            platform_msg_id: platform_msg_id.to_string(),
            bot_id: String::new(),
        }
    }

    /// Router + shared dedup wired to memory stores and a fake actor spawner
    /// (mailboxes kept alive so routing succeeds). `rate_limit_max` and the
    /// leak `detector` are the two gates these tests trip.
    struct RouterFixture {
        router: Router,
        dedup: Arc<InboundDedup>,
        _mailboxes: Arc<Mutex<Vec<MailboxReceiver<AgentMessage>>>>,
        _trigger_tx: mpsc::Sender<baybo_cron::CronTriggerEvent>,
        _response_rx: mpsc::Receiver<AgentOutput>,
    }

    fn fixture(rate_limit_max: usize, detector: LeakDetector) -> RouterFixture {
        let (response_tx, response_rx) = mpsc::channel(64);
        let supervisor = AgentSupervisor::new(response_tx);
        let sessions = Arc::new(SessionManager::new(
            Arc::new(MemorySessionStore::new()),
            Arc::new(MemorySessionFolderStore::new()),
        ));
        let mailboxes: Arc<Mutex<Vec<MailboxReceiver<AgentMessage>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let mailboxes_for_closure = Arc::clone(&mailboxes);
        let actor_spawner: ActorSpawner =
            Arc::new(move |_session, _llm, _model, _effort, _tx, _token| {
                let (sender, receiver) = mailbox::channel(16);
                mailboxes_for_closure.lock().push(receiver);
                sender
            });
        let key = EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap();
        let vault = Arc::new(SecretVault::new(key, Arc::new(MemorySecretStore::new())));
        let (trigger_tx, cron_trigger_rx) = mpsc::channel(16);
        let dedup = Arc::new(InboundDedup::new());
        let router = Router::from_config(RouterConfig {
            session_manager: sessions,
            supervisor,
            channels: Arc::new(ChannelRegistry::new()),
            security_gateway: Arc::new(SecurityGateway::new(Arc::new(detector), vault)),
            cost_manager: CostManager::new(
                Arc::new(MemoryCostStore::new()),
                HashMap::new(),
                SpendingLimits::default(),
            ),
            actor_spawner,
            turn_lifecycle: Arc::new(TurnLifecycle::new(Arc::new(MemoryTurnStore::new()))),
            cron_store: Arc::new(InMemoryCronStore::new()) as Arc<dyn CronStore>,
            agent_profiles: Arc::new(baybo_store::test_support::MemoryAgentProfileStore::new()),
            cron_trigger_rx,
            issue_run_rx: None,
            board: None,
            actor_parent_token: CancellationToken::new(),
            rate_limit: LiveRateLimit::new(rate_limit_max, Duration::from_secs(60)),
            workspace: Arc::new(baybo_workspace::WorkspacePaths::new(
                "/tmp/baybo-user-input-test",
            )),
            inbound_dedup: Arc::clone(&dedup),
        });
        RouterFixture {
            router,
            dedup,
            _mailboxes: mailboxes,
            _trigger_tx: trigger_tx,
            _response_rx: response_rx,
        }
    }

    /// The gate-rejection un-record, at the Router seam it lives on: the
    /// channel layer records the dedup key BEFORE the gates run (the echo
    /// follows the record), so a rejected send must give its key back — the
    /// client outbox retries under the same `platform_msg_id` by design, and
    /// a burned key makes the send permanently unsendable. The admitted
    /// send's key must stay burned, or a retransmission double-runs a turn.
    #[tokio::test]
    async fn a_gate_rejected_send_releases_its_dedup_key() {
        // One send per window: the second is the gate rejection under test.
        let mut fx = fixture(1, LeakDetector::with_default_rules());
        let ct = ChannelType::tui();

        // The channel layer's half, simulated: record before routing.
        assert!(fx.dedup.check_and_record(&ct, "", "msg-1"));
        fx.router
            .handle_incoming(incoming("msg-1"))
            .await
            .expect("first send is admitted and routed");

        assert!(fx.dedup.check_and_record(&ct, "", "msg-2"));
        let rejected = fx.router.handle_incoming(incoming("msg-2")).await;
        assert!(
            rejected.is_err(),
            "second send trips the 1/window rate limit"
        );

        // The rejected key was given back; the routed one stays burned.
        assert!(
            fx.dedup.check_and_record(&ct, "", "msg-2"),
            "a gate-rejected send's key must be re-admitted"
        );
        assert!(
            !fx.dedup.check_and_record(&ct, "", "msg-1"),
            "a routed send's key must stay recorded"
        );
    }

    /// The batch choke point's un-record: a whole-group bail (here the rate
    /// limit, checked once per row) must give EVERY row's key back.
    #[tokio::test]
    async fn a_gate_rejected_batch_releases_every_key() {
        let mut fx = fixture(1, LeakDetector::with_default_rules());
        let ct = ChannelType::tui();

        assert!(fx.dedup.check_and_record(&ct, "", "msg-1"));
        assert!(fx.dedup.check_and_record(&ct, "", "msg-2"));
        let rejected = fx
            .router
            .handle_incoming_batch(vec![incoming("msg-1"), incoming("msg-2")])
            .await;
        assert!(
            rejected.is_err(),
            "a 2-row batch overruns the 1/window limit"
        );

        assert!(fx.dedup.check_and_record(&ct, "", "msg-1"));
        assert!(fx.dedup.check_and_record(&ct, "", "msg-2"));
    }

    /// The per-row un-record inside the batch path: a security-dropped row is
    /// discarded with an Ok result — the choke point never sees it — so its
    /// key must be freed AT the drop, while the routed survivors stay burned.
    #[tokio::test]
    async fn a_security_dropped_batch_row_releases_only_its_own_key() {
        let mut detector = LeakDetector::new();
        detector.add_rule(LeakDetectionRule {
            name: "block_test".into(),
            pattern: Regex::new(r"TOP_SECRET_\w+").unwrap(),
            action: LeakAction::Block,
        });
        let mut fx = fixture(100, detector);
        let ct = ChannelType::tui();

        let mut blocked = incoming("msg-blocked");
        blocked.message.content = text("here is TOP_SECRET_DATA");

        assert!(fx.dedup.check_and_record(&ct, "", "msg-blocked"));
        assert!(fx.dedup.check_and_record(&ct, "", "msg-clean"));
        fx.router
            .handle_incoming_batch(vec![blocked, incoming("msg-clean")])
            .await
            .expect("the surviving row routes; the drop is per-row, not a bail");

        assert!(
            fx.dedup.check_and_record(&ct, "", "msg-blocked"),
            "the dropped row's key must be freed"
        );
        assert!(
            !fx.dedup.check_and_record(&ct, "", "msg-clean"),
            "the routed row's key must stay recorded"
        );
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
        assert!(is_in_flight(TurnStatusKind::Pending));
        assert!(is_in_flight(TurnStatusKind::InProgress));
        assert!(is_in_flight(TurnStatusKind::Stuck));
        assert!(!is_in_flight(TurnStatusKind::Completed));
        assert!(!is_in_flight(TurnStatusKind::Failed));
        assert!(!is_in_flight(TurnStatusKind::Cancelled));
    }
}
