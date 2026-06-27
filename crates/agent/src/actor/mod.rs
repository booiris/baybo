//! Actor model + supervision: one `AgentActor` per session, supervised
//! by [`AgentSupervisor`](crate::actor::supervisor::AgentSupervisor),
//! routed to by [`Router`](crate::actor::router::Router), and
//! checkpointed via [`DurableActorState`](crate::actor::state::DurableActorState).

pub mod mailbox;
pub mod router;
pub mod runner;
pub mod state;
pub mod subagent;
pub mod supervisor;

use crate::runtime::agent_loop::InterjectionSource;
use baybo_channels::{
    AgentEvent, AgentOutput, COMPACT_COMMAND, IncomingMessage, NoticeLevel, OutgoingMessage,
};
use baybo_job::JobInput;
use baybo_model::{ContentBlock, ControlEventKind, LlmEntryName, PendingBackgroundResult};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Hard cap on `session.state.pending_background_results`. A parent
/// that stays idle while many background subagents finish would
/// otherwise grow this vec without bound — both in memory and on
/// the persisted row. Once the cap is reached the oldest entry is
/// dropped (its content still lives in the child session's trace).
const MAX_PENDING_BACKGROUND_RESULTS: usize = 64;

/// How long after sealing a subagent group the barrier waits for all
/// members before firing partial + dissolving the cohort (still-running
/// members then deliver individually). Generous — group members are real
/// background subagents.
const GROUP_TIMEOUT_MINUTES: i64 = 30;

/// Exponential-backoff retry schedule for a FAILED `SubagentNotification`
/// turn. When the turn errors (provider / cost / cancel) and the session is
/// idle, the actor retries on this backoff so a fire-and-forget completion is
/// still reported during the idle window. There is **no attempt cap** — a
/// delivered completion is never dropped, so the actor retries indefinitely
/// (each step doubling, capped at `NOTIFY_RETRY_MAX_BACKOFF`) until it
/// succeeds; an inbound message resets the schedule. Trade-off: a session
/// whose notification turn keeps failing stays resident (each retry bumps
/// `last_active`, so the idle reaper won't reclaim it) rather than dropping
/// the completion — the result is also buffered + persisted throughout.
const NOTIFY_RETRY_INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);
const NOTIFY_RETRY_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(300);

/// Surfaced (as a `Notice`) when a *user* turn yields a blank reply — the
/// user is waiting, so acknowledge rather than push an empty bubble. Non-user
/// turns (cron, subagent notification) silently suppress a blank reply.
const EMPTY_USER_REPLY_NOTICE: &str =
    "The assistant did not produce a response to your message. Please try again or rephrase.";

use crate::actor::state::{DurableActorState, VolatileResources};
use crate::actor::supervisor::ActorRegistryGuard;

/// Messages that can be sent to an AgentActor.
#[derive(Debug, Clone)]
pub enum AgentMessage {
    /// A user sent a message.
    UserInput(Box<IncomingMessage>),
    /// A client-batched group of user messages for this session, delivered as
    /// one mailbox item so they run as a single coalesced turn regardless of
    /// per-message router timing (the web "send every queued message at once"
    /// path). Equivalent to a run of `UserInput`s the actor would have
    /// coalesced anyway — `handle_merged_user_turn` appends each as its own
    /// transcript row and answers the group with one reply. Non-slash only
    /// (the gateway never batches a slash command).
    UserInputBatch(Vec<IncomingMessage>),
    /// A cron job fired.
    CronTrigger { job_id: String, prompt: String },
    /// A subagent was spawned. Carries the initial prompt assembled by
    /// `Router::handle_subagent_spawn` and the parent's `JobId` for
    /// lineage. The child actor runs `agent_loop.run` with `JobInput::Spawned`;
    /// the job records the child session's root trigger as its `origin`
    /// (subagents inherit the parent's trigger — cron / system — via
    /// `create_spawned_session`), with no payload/trigger pairing constraint.
    SubagentSpawned {
        initial_message: Box<IncomingMessage>,
        parent_job_id: baybo_model::JobId,
    },
    /// A `background: true` subagent dispatched from this session
    /// reached a terminal state. The wait task posts this to the parent
    /// actor's mailbox; it is buffered on
    /// `session.state.pending_background_results` and, once no
    /// higher-priority work is queued, drained into one autonomous
    /// `SubagentNotification` turn.
    BackgroundJobFinished(Box<PendingBackgroundResult>),
    /// Re-pin the session's LLM (chat per-session model switch). `llm`
    /// is the `baybo.json` entry name to resolve against, or `None` to
    /// revert to `default-llm`. The handler re-pins the live loop in
    /// place and persists `session.state.last_llm` so the choice
    /// survives eviction. Processed at a turn boundary (the mailbox is
    /// drained sequentially), so it never swaps the model mid-turn —
    /// it takes effect on the next turn. Routed by the gateway's
    /// `PUT /v1/chat/sessions/{id}/model` via [`AgentSupervisor::route`].
    SetModel { llm: Option<LlmEntryName> },
    /// Stop this actor. Lowest mailbox priority — every queued
    /// `UserInput` / `BackgroundJobFinished` drains first, then the actor
    /// trips its `actor_token` and exits. (The session row lives on; only
    /// the in-memory actor stops.)
    ActorStop,
}

impl mailbox::Prioritized for AgentMessage {
    fn priority(&self) -> mailbox::MessagePriority {
        use mailbox::MessagePriority;
        match self {
            AgentMessage::UserInput(_)
            | AgentMessage::UserInputBatch(_)
            | AgentMessage::CronTrigger { .. }
            | AgentMessage::SubagentSpawned { .. }
            | AgentMessage::SetModel { .. } => MessagePriority::Trigger,
            AgentMessage::BackgroundJobFinished(_) => MessagePriority::BackgroundJobFinished,
            AgentMessage::ActorStop => MessagePriority::Stop,
        }
    }
}

/// A `SubagentNotification` reply is suppressed (not sent to the channel)
/// when it carries no non-whitespace text — the model's only, implicit,
/// way to stay quiet (there is no `<no_output/>` sentinel).
fn is_blank_reply(content: &[ContentBlock]) -> bool {
    content.iter().all(|b| match b {
        ContentBlock::Text(t) => t.trim().is_empty(),
        _ => false,
    })
}

/// Mirrors the gateway slash dispatcher's tolerance for casing and
/// trailing arguments so a user typing `/Compact extra` from any
/// channel hits the same control path.
fn is_compact_command(content: &[ContentBlock]) -> bool {
    let Some(ContentBlock::Text(text)) = content.first() else {
        return false;
    };
    let Some(rest) = text.trim().strip_prefix('/') else {
        return false;
    };
    let token = rest.split_whitespace().next().unwrap_or("");
    token.eq_ignore_ascii_case(COMPACT_COMMAND.trim_start_matches('/'))
}

/// Whether `content` leads with any slash command (`/compact`, `/<skill>`,
/// or any `/<token>`). Such a message is a hard boundary for `UserInput`
/// coalescing: it runs on its own so the leading `/` stays at content
/// position 0, where compact detection and the agent loop's skill
/// detection (`detect_slash_invocation`) look. Purely syntactic — it does
/// not resolve the token against any registry.
fn is_slash_command(content: &[ContentBlock]) -> bool {
    let Some(ContentBlock::Text(text)) = content.first() else {
        return false;
    };
    text.trim_start()
        .strip_prefix('/')
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| !c.is_whitespace())
}

/// True for a `UserInput` eligible to be folded into a running turn: a
/// **non-slash** user message. Both pop sites — the coalescer in
/// [`AgentActor::run`] and the in-turn interjection drain
/// ([`MailboxInterjections::drain_injectable`]) — must use this *exact*
/// predicate: that sameness is what makes a queued slash command a hard barrier
/// for both, so a message queued behind a slash can't be pulled past it. (The
/// two sites drifting is exactly how the slash-barrier broke once before; keep
/// them routed through here.)
fn is_coalescable_user_input(msg: &AgentMessage) -> bool {
    matches!(msg, AgentMessage::UserInput(inc) if !is_slash_command(&inc.message.content))
}

/// Adapts the actor's mailbox into an
/// [`InterjectionSource`](crate::runtime::agent_loop::InterjectionSource) for
/// the running agent loop: drains the leading run of **non-slash** `UserInput`s
/// queued mid-turn, leaving a queued slash command / `BackgroundJobFinished` /
/// `ActorStop` in place for the actor's normal dispatch. `try_recv_if` stops at
/// the first non-injectable message, so coalescing boundaries and priority
/// ordering are preserved. See `docs/mid-turn-user-interjection.md`.
struct MailboxInterjections<'a> {
    rx: &'a mut mailbox::MailboxReceiver<AgentMessage>,
}

impl crate::runtime::agent_loop::InterjectionSource for MailboxInterjections<'_> {
    fn drain_injectable(&mut self) -> Vec<Vec<ContentBlock>> {
        let mut out = Vec::new();
        while let Some(AgentMessage::UserInput(inc)) =
            self.rx.try_recv_if(is_coalescable_user_input)
        {
            out.push(inc.message.content);
        }
        out
    }

    /// On a `/stop`, drop the leading run of queued client follow-ups so they
    /// don't run after the stop — including a coalesced `UserInputBatch`, which
    /// `is_coalescable_user_input` (mid-turn injection, single-message only)
    /// deliberately excludes. Without this a batch queued behind the cancelled
    /// turn would survive and fire as a fresh turn. Still stops at the first
    /// slash / other-kind message, preserving the same barrier as the drain.
    fn discard_pending(&mut self) {
        while self
            .rx
            .try_recv_if(|m| {
                is_coalescable_user_input(m) || matches!(m, AgentMessage::UserInputBatch(_))
            })
            .is_some()
        {}
    }
}

/// One actor per session. Receives messages sequentially through its mailbox.
///
/// State is split into two halves by lifetime class (see
/// [`crate::actor::state`]):
/// - `durable` — must survive eviction; persisted via the session store.
/// - `volatile` — rebuilt from `durable` + the runtime each time the
///   actor is spawned.
pub struct AgentActor {
    durable: DurableActorState,
    volatile: VolatileResources,
}

impl AgentActor {
    /// Construct an actor from its durable and volatile halves.
    ///
    /// Production wiring goes through [`crate::actor::router::ActorSpawner`],
    /// which builds the [`VolatileResources`] from the per-process
    /// dependency graph and either creates a fresh [`DurableActorState`]
    /// or hydrates one from the session store.
    pub fn from_parts(durable: DurableActorState, volatile: VolatileResources) -> Self {
        Self { durable, volatile }
    }

    /// Run the actor's message processing loop.
    pub async fn run(mut self, mut mailbox: mailbox::MailboxReceiver<AgentMessage>) {
        let session_id = self.durable.session.id.clone();
        info!(session_id = %session_id, "agent actor started");

        // Self-deregistration on any exit path — Shutdown message,
        // mailbox close, or panic. Without this guard the supervisor's
        // `actors` map would keep the dead handle indefinitely.
        let _registry_guard = self
            .volatile
            .supervisor
            .take()
            .map(|s| ActorRegistryGuard::new(s, session_id.clone()));

        // Cold-start hydration: pull any persisted transcript out of
        // the store before processing the first message. No-ops for
        // fresh sessions (cron fires, subagent spawns, brand-new
        // user sessions) and for test harnesses that don't wire a
        // store; failures log and fall through to an empty transcript.
        self.volatile
            .agent_loop
            .restore_transcript_from_store()
            .await;

        // A failed `SubagentNotification` turn leaves the buffer non-empty;
        // rather than wait for the next inbound message, retry on an
        // exponential backoff (capped at `NOTIFY_RETRY_MAX_BACKOFF`) so a
        // fire-and-forget completion is still reported during the idle
        // window. No attempt cap — a delivered completion is never dropped,
        // so the actor keeps retrying until it succeeds. A real inbound
        // message wins the race (biased) and resets the backoff.
        let mut notify_backoff = NOTIFY_RETRY_INITIAL_BACKOFF;
        loop {
            // Stay on the timed path while there are pending results to retry
            // OR open barrier cohorts whose group timeout must be enforced
            // even with no inbound message.
            let next = if !self
                .durable
                .session
                .state
                .pending_background_results
                .is_empty()
                || self.has_open_groups()
            {
                tokio::select! {
                    biased;
                    m = mailbox.recv() => m,
                    _ = tokio::time::sleep(notify_backoff) => {
                        // Release any cohort that completed or hit its timeout
                        // into the buffer, then drain. A cohort that times out
                        // with zero results buffers nothing, so the
                        // notification's own persist won't fire — persist the
                        // group-map change here so the sweep survives a later
                        // rehydration.
                        if self.check_groups() {
                            self.persist_session_state_after_pending_change("group_swept")
                                .await;
                        }
                        self.run_subagent_notification().await;
                        if self
                            .durable
                            .session
                            .state
                            .pending_background_results
                            .is_empty()
                            && !self.has_open_groups()
                        {
                            notify_backoff = NOTIFY_RETRY_INITIAL_BACKOFF;
                        } else {
                            notify_backoff = (notify_backoff * 2).min(NOTIFY_RETRY_MAX_BACKOFF);
                        }
                        continue;
                    }
                }
            } else {
                mailbox.recv().await
            };
            let Some(msg) = next else {
                break;
            };
            // A real message resets the notification backoff (fresh schedule).
            notify_backoff = NOTIFY_RETRY_INITIAL_BACKOFF;
            match msg {
                AgentMessage::ActorStop => {
                    debug!(session_id = %session_id, "actor stopping");
                    // Session-end memory consolidation. Detached on the
                    // runtime root so it survives the `actor_token.cancel()`
                    // below; gated to user-facing sessions only (subagents,
                    // maintenance, system-triggered actors send `ActorStop`
                    // too but are not user-session endings — see
                    // `should_fire_session_end`). No-op when no memory is
                    // wired (default in production today).
                    self.volatile.agent_loop.spawn_session_end_write(
                        &self.volatile.span_recorder,
                        &self.durable.session,
                    );
                    // Cancelling our `actor_token` cascades into every
                    // child we spawned — a subagent actor derives its
                    // `actor_token` from ours via the parent context the
                    // spawner threads through, so parent shutdown reaches
                    // it with no explicit dispatch.
                    self.volatile.actor_token.cancel();
                    break;
                }
                // Coalesce a leading run of NON-slash `UserInput`s already
                // queued behind this one into a single turn (rapid bursts the
                // actor was too busy to take one at a time). `try_recv_if` pops
                // ONLY non-slash `UserInput`s, so a queued slash command (or any
                // other message kind) stays at the HEAD of the mailbox. That
                // serves two roles: the coalescing hard boundary (the slash's
                // leading `/` stays at content position 0 for compact / skill
                // detection), AND a barrier the in-turn interjection drain
                // (`MailboxInterjections`, same predicate) cannot pop past — so
                // a message queued *behind* a slash can never be pulled into
                // this turn ahead of it. The boundary is served by the next
                // `recv()` iteration, not popped out of order here.
                AgentMessage::UserInput(incoming)
                    if !is_slash_command(&incoming.message.content) =>
                {
                    let mut batch = vec![*incoming];
                    while let Some(AgentMessage::UserInput(inc)) =
                        mailbox.try_recv_if(is_coalescable_user_input)
                    {
                        batch.push(*inc);
                    }
                    if let Err(e) = self.handle_merged_user_turn(batch, &mut mailbox).await {
                        error!(
                            session_id = %session_id,
                            error = %e,
                            "failed to handle user input"
                        );
                    }
                }
                // A pre-coalesced batch (the gateway already grouped these as one
                // atomic intake item). Run it as a single merged turn; also pull
                // any further non-slash `UserInput`s queued behind it, same as the
                // single-message path, so a straggler that landed after the batch
                // still folds into this turn rather than starting another.
                AgentMessage::UserInputBatch(mut batch) => {
                    while let Some(AgentMessage::UserInput(inc)) =
                        mailbox.try_recv_if(is_coalescable_user_input)
                    {
                        batch.push(*inc);
                    }
                    if let Err(e) = self.handle_merged_user_turn(batch, &mut mailbox).await {
                        error!(
                            session_id = %session_id,
                            error = %e,
                            "failed to handle user input batch"
                        );
                    }
                }
                other => {
                    self.dispatch_one(other).await;
                }
            }
            // The turn that just ran finished dispatching its grouped spawns,
            // so seal their cohorts: membership is now final and the barrier
            // (and its timeout) can fire.
            self.seal_open_groups().await;
            // Surface any buffered background-subagent results as their
            // own turn once nothing higher-priority remains queued.
            self.maybe_run_subagent_notification(&mailbox).await;
        }
        info!(session_id = %session_id, "agent actor stopped");
    }

    /// Handle one mailbox message — every kind except `ActorStop`, which
    /// the run loop owns because it must break the loop. Shared by the
    /// non-coalesced path and the deferred follow-up of a `UserInput`
    /// coalescing run.
    async fn dispatch_one(&mut self, msg: AgentMessage) {
        let session_id = self.durable.session.id.clone();
        match msg {
            AgentMessage::UserInput(incoming) => {
                if let Err(e) = self.handle_user_input(*incoming).await {
                    error!(session_id = %session_id, error = %e, "failed to handle user input");
                }
            }
            AgentMessage::CronTrigger { job_id, prompt } => {
                debug!(session_id = %session_id, job_id = %job_id, "received cron trigger");
                if let Err(e) = self.dispatch_cron_prompt(&prompt, &job_id).await {
                    error!(session_id = %session_id, job_id = %job_id, error = %e, "failed to handle cron trigger");
                }
            }
            AgentMessage::SubagentSpawned {
                initial_message,
                parent_job_id,
            } => {
                if let Err(e) = self
                    .handle_subagent_spawned(*initial_message, parent_job_id)
                    .await
                {
                    error!(session_id = %session_id, error = %e, "failed to handle subagent spawn");
                }
            }
            AgentMessage::BackgroundJobFinished(pending) => {
                self.handle_background_finished(*pending).await;
            }
            AgentMessage::SetModel { llm } => {
                self.handle_set_model(llm);
            }
            AgentMessage::UserInputBatch(_) => {
                // The run loop owns `UserInputBatch` (it needs `&mut mailbox` to
                // coalesce + run the merged turn); reaching here would be a
                // routing bug. Defensive no-op.
                warn!(session_id = %session_id, "UserInputBatch reached dispatch_one; ignoring");
            }
            AgentMessage::ActorStop => {
                // The run loop owns `ActorStop` (it must break); reaching
                // here would be a routing bug. Defensive no-op.
                warn!(session_id = %session_id, "ActorStop reached dispatch_one; ignoring");
            }
        }
    }

    /// Run the agent loop. Terminal-state notification is published by
    /// `JobLifecycle` itself on the broadcast bus
    /// (`subscribe_lifecycle_events`); the actor no longer emits a
    /// piggy-back signal on the response channel. Used by every
    /// handler that delegates job lifecycle to `agent_loop.run`
    /// (UserInput, SubagentSpawned, cron prompt dispatch). Returns
    /// the loop's response on `Ok`; caller is responsible for sending
    /// it to the response channel.
    /// Run the loop on the session's *current* context. The triggering
    /// message must already be appended (the callers do this via
    /// `AgentLoop::append_user_message` / `append_cron_fire` /
    /// `append_subagent_notification`) so framing lives in `baybo-context`
    /// and the loop just iterates.
    async fn run_agent_loop(
        &mut self,
        job_input: JobInput,
        parent_job_id: Option<baybo_model::JobId>,
        delta_tx: Option<mpsc::Sender<AgentOutput>>,
        interjections: Option<&mut dyn crate::runtime::agent_loop::InterjectionSource>,
        // Set to whether this turn ended via cancellation (`/stop` / shutdown),
        // so the user-turn caller can drop queued interjections a `/stop` would
        // otherwise leave to run as follow-up turns. Caught after the loop fully
        // returns, so it covers every cancellation path (incl. mid-LLM-call).
        stopped_out: Option<&mut bool>,
    ) -> anyhow::Result<OutgoingMessage> {
        let is_user_turn = matches!(job_input.input_kind(), baybo_job::JobInputKind::UserChat);
        // Kept so the error path below can tell a user `/stop` (token
        // tripped via the job cancel) apart from a genuine failure.
        let turn_token = self.volatile.actor_token.child_token();
        // The actor emits no `TurnState`: both edges are projected from the
        // job store by `spawn_turn_state_projector` — the start edge from
        // this turn's job `start()` (`Pending → InProgress`) inside
        // `agent_loop.run`, the end edge from its terminal transition. So
        // chat turn-activity has a single producer sourced from the one
        // truth, and the per-`Subscribe` snapshot can't disagree with it.
        let result = self
            .volatile
            .agent_loop
            .run(
                &mut self.durable.session,
                job_input,
                &self.volatile.job_lifecycle,
                &self.volatile.span_recorder,
                parent_job_id,
                delta_tx,
                turn_token.clone(),
                interjections,
            )
            .await;
        if let Some(out) = stopped_out {
            *out = turn_token.is_cancelled();
        }
        // A user is waiting on this turn — a genuine failure must surface as
        // a terminal notice, not silence (the log line alone leaves the chat
        // dangling on its last progress frame). Cancellation stays quiet:
        // `/stop` already acknowledged with its own notice. Non-user turns
        // keep their own policies (cron logs, subagent-notification retries).
        if is_user_turn
            && !turn_token.is_cancelled()
            && let Err(e) = &result
        {
            let notice = AgentOutput {
                session_id: self.durable.session.id.clone(),
                user_id: self.durable.session.user.id.clone(),
                channel: self.durable.session.channel.clone(),
                event: AgentEvent::Notice {
                    level: NoticeLevel::Error,
                    text: format!("The turn failed before producing a reply: {e}"),
                },
            };
            self.send_response(notice, "user_turn_failed").await;
        }
        result
    }

    /// Dispatch a fired cron job through the agent loop and send the
    /// response to the output channel.
    ///
    /// The cron fire mints a Cron-rooted session, so the job records
    /// `origin = Cron`. The content the LLM sees is framed + appended by
    /// `AgentLoop::append_cron_fire` (which uses `baybo_context::prompts::cron`)
    /// so the model treats it as a task to perform now rather than a live
    /// user message.
    async fn dispatch_cron_prompt(&mut self, prompt: &str, job_id: &str) -> anyhow::Result<()> {
        let job_input = JobInput::Cron {
            action_payload: serde_json::json!({
                "cron_job_id": job_id,
                "prompt": prompt,
            }),
        };
        self.volatile
            .agent_loop
            .append_cron_fire(job_id, prompt)
            .await?;
        let response = self
            .run_agent_loop(job_input, None, None, None, None)
            .await?;
        if is_blank_reply(&response.content) {
            debug!(
                session_id = %self.durable.session.id,
                "cron turn produced no output; suppressing send"
            );
        } else {
            self.send_response(response.into(), "cron").await;
        }
        Ok(())
    }

    async fn handle_user_input(&mut self, incoming: IncomingMessage) -> anyhow::Result<()> {
        let sent_at = incoming.message.timestamp;
        let content = incoming.message.content;
        if is_compact_command(&content) {
            return self
                .handle_compact(slash_command_text(&content), sent_at)
                .await;
        }
        // Background `spawn_subagent` results are NOT folded into the
        // user's turn — they run as their own `SubagentNotification` turn
        // (scheduled by `maybe_run_subagent_notification`) so the user's
        // turn keeps a clean leading `/command` for slash detection.
        //
        // Pass a clone of the response channel so the loop can stream
        // text deltas as `AgentEvent::AnswerDelta` while the final assembled
        // message still flows through the normal path.
        let response_tx = self.volatile.response_tx.clone();
        self.volatile
            .agent_loop
            .append_user_message(content.clone())
            .await?;
        // Single slash-command turn (the non-slash common path is handled by
        // `handle_merged_user_turn`). Slash turns do not drain mid-turn
        // interjections — `None` keeps the leading-`/` semantics simple — so a
        // message sent during a `/skill` turn is served as the next turn.
        let response = self
            .run_agent_loop(
                JobInput::UserChat { content },
                None,
                Some(response_tx),
                None,
                None,
            )
            .await?;
        self.send_user_reply(response).await;
        Ok(())
    }

    /// Run one `UserChat` turn over a coalesced batch of `UserInput`s (a
    /// rapid burst the actor was too busy to take one at a time). Each
    /// message is kept as its **own** `Role::User` transcript row — the
    /// leading ones are appended ahead of the turn, the last becomes the
    /// turn's user content — so the stored transcript stays faithful to
    /// what the user actually sent; `merge_for_llm` collapses the
    /// consecutive rows into one message for the provider call. The job
    /// record carries the combined content for provenance. One reply
    /// answers the batch. Slash messages never reach here — they are split
    /// out as their own turns by the caller.
    async fn handle_merged_user_turn(
        &mut self,
        mut batch: Vec<IncomingMessage>,
        mailbox: &mut mailbox::MailboxReceiver<AgentMessage>,
    ) -> anyhow::Result<()> {
        // Batch always holds at least the message that triggered the turn.
        let Some(last) = batch.pop() else {
            return Ok(());
        };
        let mut combined: Vec<ContentBlock> = Vec::new();
        for incoming in &batch {
            combined.extend(incoming.message.content.iter().cloned());
        }
        combined.extend(last.message.content.iter().cloned());
        // Append every message as its own row ahead of the turn (the last
        // included) so the loop iterates the current context; the combined
        // content rides in `JobInput` for the job record only.
        for incoming in batch {
            self.volatile
                .agent_loop
                .append_user_message(incoming.message.content)
                .await?;
        }
        self.volatile
            .agent_loop
            .append_user_message(last.message.content)
            .await?;
        let response_tx = self.volatile.response_tx.clone();
        // Let the loop drain user messages that arrive *during* this turn and
        // inject them at tool boundaries (see `MailboxInterjections` /
        // docs/mid-turn-user-interjection.md). The coalesced burst above is
        // already appended; this only pulls messages that land after the turn
        // starts. Anything still queued at turn-end falls to the next turn.
        let mut interjections = MailboxInterjections { rx: mailbox };
        let mut stopped = false;
        let result = self
            .run_agent_loop(
                JobInput::UserChat { content: combined },
                None,
                Some(response_tx),
                Some(&mut interjections),
                Some(&mut stopped),
            )
            .await;
        // A `/stop` halts the whole pipeline: drop any interjections the client
        // already sent that are still queued in the mailbox, so they don't run
        // as follow-up turns once we return to the actor's main loop. Stops at
        // the first slash / other-kind message — the same barrier the coalescer
        // and the in-turn interjection drain use.
        if stopped {
            interjections.discard_pending();
        }
        let response = result?;
        self.send_user_reply(response).await;
        Ok(())
    }

    /// Idempotent on `handle_id` — the wait task can in principle
    /// publish twice (mailbox retry, manual recovery) and we don't
    /// want the notification to list it twice. Capped at
    /// `MAX_PENDING_BACKGROUND_RESULTS` with drop-oldest semantics so a
    /// parent that stays idle while many backgrounds finish can't
    /// grow the persisted row without bound.
    async fn handle_background_finished(&mut self, pending: PendingBackgroundResult) {
        if self.background_result_known(&pending.handle_id) {
            debug!(
                session_id = %self.durable.session.id,
                handle_id = %pending.handle_id,
                "duplicate BackgroundJobFinished for handle; ignoring"
            );
            return;
        }
        debug!(
            session_id = %self.durable.session.id,
            handle_id = %pending.handle_id,
            label = %pending.label,
            "buffered background job result"
        );
        // Route a grouped member into its still-open cohort; everything else
        // — non-grouped jobs, and grouped members whose cohort already
        // released or dissolved — goes straight to the notification buffer.
        let grouped = pending.group.as_ref().and_then(|g| {
            self.durable
                .session
                .state
                .background_groups
                .contains_key(g)
                .then(|| g.clone())
        });
        match grouped {
            Some(g) => {
                if let Some(state) = self.durable.session.state.background_groups.get_mut(&g) {
                    state.results.push(pending);
                }
            }
            None => self.buffer_pending_result(pending),
        }
        self.check_groups();
        self.persist_session_state_after_pending_change("background_finished")
            .await;
    }

    /// Whether a result with this handle is already buffered (in the
    /// notification queue or any group cohort) — dedup across re-delivery.
    fn background_result_known(&self, handle_id: &str) -> bool {
        let state = &self.durable.session.state;
        state
            .pending_background_results
            .iter()
            .any(|p| p.handle_id == handle_id)
            || state
                .background_groups
                .values()
                .any(|g| g.results.iter().any(|p| p.handle_id == handle_id))
    }

    /// Push a result into the notification buffer, capped drop-oldest.
    fn buffer_pending_result(&mut self, pending: PendingBackgroundResult) {
        let buffer = &mut self.durable.session.state.pending_background_results;
        if buffer.len() >= MAX_PENDING_BACKGROUND_RESULTS {
            let dropped = buffer.remove(0);
            warn!(
                session_id = %self.durable.session.id,
                dropped_handle_id = %dropped.handle_id,
                cap = MAX_PENDING_BACKGROUND_RESULTS,
                "pending background buffer full; dropping oldest entry"
            );
        }
        buffer.push(pending);
    }

    /// Seal every still-open barrier cohort at a turn boundary: membership is
    /// final once the dispatching turn ends, so the barrier may fire. Starts
    /// each group's timeout clock. Persists if anything changed.
    async fn seal_open_groups(&mut self) {
        let now = chrono::Utc::now();
        let mut changed = false;
        for g in self.durable.session.state.background_groups.values_mut() {
            if !g.sealed {
                g.sealed = true;
                g.sealed_at = Some(now);
                changed = true;
            }
        }
        if changed {
            // A cohort whose members all finished mid-turn is complete the
            // moment it seals — release it before the next drain.
            self.check_groups();
            self.persist_session_state_after_pending_change("group_sealed")
                .await;
        }
    }

    /// Whether any barrier cohort is still open — keeps the idle loop awake so
    /// the group timeout is enforced even with no inbound messages.
    fn has_open_groups(&self) -> bool {
        !self.durable.session.state.background_groups.is_empty()
    }

    /// Release every complete (`results.len() >= expected`) or timed-out
    /// cohort into the notification buffer. A timed-out cohort fires partial
    /// (its finished members) and dissolves — still-running members revert to
    /// individual delivery, since their later result finds no cohort and
    /// buffers directly. No-op while a cohort is still filling.
    fn check_groups(&mut self) -> bool {
        let now = chrono::Utc::now();
        let timeout = chrono::Duration::minutes(GROUP_TIMEOUT_MINUTES);
        let ready: Vec<String> = self
            .durable
            .session
            .state
            .background_groups
            .iter()
            .filter(|(_, g)| g.is_ready(now, timeout))
            .map(|(name, _)| name.clone())
            .collect();
        let removed = !ready.is_empty();
        for name in ready {
            let Some(g) = self.durable.session.state.background_groups.remove(&name) else {
                continue;
            };
            if g.is_partial() {
                debug!(
                    session_id = %self.durable.session.id,
                    group = %name,
                    have = g.results.len(),
                    expected = g.expected,
                    "group timed out; partial-firing and dissolving"
                );
            }
            for r in g.results {
                self.buffer_pending_result(r);
            }
        }
        removed
    }

    /// Re-pin this session's LLM in place (chat per-session model switch)
    /// so the swap lands on the next turn. **In-memory only** — durability
    /// is the caller's responsibility: the gateway's `PUT
    /// /v1/chat/sessions/:id/model` does a targeted, race-free
    /// `set_last_llm` column write *before* routing this message, and that
    /// column (not the JSON blob) is what a later spawn reads back via
    /// `get`. Persisting here through a full-blob `save` would instead be
    /// clobberable by a concurrent `touch`, which is the bug this split
    /// avoids. The mailbox drains sequentially, so a `SetModel` queued
    /// behind an in-flight turn applies once that turn ends, never
    /// mid-turn. The gateway validated `llm` against the pool; even a
    /// stranded name degrades safely (`LlmClientPool::resolve` falls back
    /// to the default).
    fn handle_set_model(&mut self, llm: Option<LlmEntryName>) {
        debug!(
            session_id = %self.durable.session.id,
            llm = ?llm,
            "re-pinning session LLM",
        );
        // Keep the in-memory view consistent (the persisted column is
        // authoritative on the next `get`); then swap the live loop.
        self.durable.session.state.last_llm = llm.clone();
        self.volatile.agent_loop.set_initial_llm(llm);
    }

    /// Write the actor's current `durable.session` back to the
    /// session store so the latest `pending_background_results` survives
    /// eviction. Called only by the background-subagent paths today.
    /// Logs a warn on storage error rather than failing the surrounding
    /// handler — losing the persisted copy degrades to "delivered to
    /// the live actor only" rather than "delivered nowhere", which is
    /// strictly worse than the v1 (mailbox-only) baseline. Updates
    /// `last_active` in passing so the reaper doesn't immediately
    /// re-target the actor after the counter clears.
    async fn persist_session_state_after_pending_change(&mut self, context: &'static str) {
        self.durable.session.last_active = chrono::Utc::now();
        if let Err(e) = self
            .volatile
            .session_manager
            .store()
            .save(&self.durable.session)
            .await
        {
            warn!(
                session_id = %self.durable.session.id,
                context = %context,
                error = %e,
                "failed to persist session.state after pending-subagent change; live actor still has the latest copy in memory"
            );
        }
    }

    /// Surface buffered background-subagent results as their own turn —
    /// but only when no higher-priority work is queued: a `Trigger`
    /// (UserInput) must run first, and another queued `BackgroundJobFinished`
    /// is folded in first (merge). An empty queue or a lowest-priority
    /// `ActorStop` means "drain now".
    async fn maybe_run_subagent_notification(
        &mut self,
        mailbox: &mailbox::MailboxReceiver<AgentMessage>,
    ) {
        if self
            .durable
            .session
            .state
            .pending_background_results
            .is_empty()
        {
            return;
        }
        if matches!(
            mailbox.peek_priority(),
            Some(p) if p >= mailbox::MessagePriority::BackgroundJobFinished
        ) {
            return;
        }
        self.run_subagent_notification().await;
    }

    /// Run the merged background-subagent notification as its own
    /// main-path turn (same system prompt + toolset → prompt cache
    /// unchanged). The reply is sent proactively; an empty/whitespace
    /// reply is suppressed.
    async fn run_subagent_notification(&mut self) {
        // Establish the system prompt before the snapshot below. It is
        // persisted by `ensure_seeded`, so if the snapshot were taken before it
        // (on a fresh session the prior context is empty), a rollback would drop
        // the just-persisted system row in-memory and the next retry would
        // re-seed and re-persist it. Idempotent mid-session, and infallible.
        self.volatile.agent_loop.ensure_system_prompt_seeded().await;
        // Take the results but keep them in hand: the turn below is
        // fallible (provider error, cost rejection, cancellation), and a
        // delivered completion must not be lost if it fails. The actor is
        // single-threaded, so nothing else mutates the buffer while the
        // turn runs.
        let pending = std::mem::take(&mut self.durable.session.state.pending_background_results);
        if pending.is_empty() {
            return;
        }
        let content = baybo_context::prompts::subagent::build_notification_content(&pending);
        // Commit the drained (now-empty) buffer to the row BEFORE the
        // fallible turn: a crash mid-turn must not leave the results in the
        // row to be replayed as a DUPLICATE notification on restart. On an
        // in-process turn failure we re-buffer below, so a transient error
        // still retries (the actor is single-threaded — nothing else
        // mutates the buffer while the turn runs).
        self.persist_session_state_after_pending_change("subagent_notification_drained")
            .await;
        // No delta streaming: the empty-output decision is made on the
        // assembled reply, so nothing may have been streamed already.
        //
        // Snapshot the transcript first: the notification's synthetic prompt
        // is appended in-memory only (not persisted), so a failed turn must
        // roll back to here before the retry rebuilds it — otherwise the live
        // context stacks a copy per attempt under the infinite-backoff retry.
        let context_snapshot = self.volatile.agent_loop.context_snapshot();
        // Append the synthetic prompt in-memory *after* the snapshot so a
        // failed turn rolls it back; the durable buffer is the source of truth.
        self.volatile
            .agent_loop
            .append_subagent_notification(content.clone());
        let result = self
            .run_agent_loop(
                JobInput::SubagentNotification { content },
                None,
                None,
                None,
                None,
            )
            .await;
        match result {
            Ok(response) => {
                if is_blank_reply(&response.content) {
                    debug!(
                        session_id = %self.durable.session.id,
                        "subagent-notification produced no output; suppressing send"
                    );
                    return;
                }
                self.send_response(response.into(), "subagent_notification")
                    .await;
            }
            Err(e) => {
                error!(
                    session_id = %self.durable.session.id,
                    error = %e,
                    "subagent-notification turn failed; restoring pending results for retry"
                );
                // Drop the in-memory synthetic row (and any partial turn
                // state) this attempt appended, so the retry doesn't stack a
                // second copy in the live context.
                self.volatile.agent_loop.restore_context(context_snapshot);
                // Restore the drained results so the next drain retries them —
                // the child trace alone would never resurface.
                let mut restored = pending;
                restored.append(&mut self.durable.session.state.pending_background_results);
                self.durable.session.state.pending_background_results = restored;
                self.persist_session_state_after_pending_change("subagent_notification_restore")
                    .await;
            }
        }
    }

    /// Send a user-turn reply. A blank reply (no non-whitespace text) is
    /// anomalous for a user turn — the user is waiting — so surface a
    /// fallback `Notice` rather than push an empty bubble. (Non-user turns —
    /// cron, subagent notification — silently suppress a blank reply.)
    async fn send_user_reply(&self, response: OutgoingMessage) {
        if is_blank_reply(&response.content) {
            warn!(
                session_id = %self.durable.session.id,
                "user turn produced an empty reply; surfacing fallback notice"
            );
            // Record the fallback as an out-of-band control event so a reload
            // doesn't show a bare user turn with no reply.
            if let Some(after) = self.current_after_ordinal().await {
                self.persist_control_event(
                    after,
                    ControlEventKind::NoticeWarn,
                    EMPTY_USER_REPLY_NOTICE,
                    chrono::Utc::now(),
                )
                .await;
            }
            let notice = AgentOutput {
                session_id: self.durable.session.id.clone(),
                user_id: self.durable.session.user.id.clone(),
                channel: self.durable.session.channel.clone(),
                event: AgentEvent::Notice {
                    level: NoticeLevel::Warn,
                    text: EMPTY_USER_REPLY_NOTICE.to_string(),
                },
            };
            self.send_response(notice, "user_empty_fallback").await;
            return;
        }
        self.send_response(response.into(), "user").await;
    }

    /// `/compact` is a control command, not an assistant turn, so the
    /// confirmation goes back as a `Notice` rather than an assistant `Message`.
    /// The command echo and the confirmation are also recorded as out-of-band
    /// control events (`session_control_events`, kept out of the LLM transcript)
    /// so a reload shows both. `sent_at` is when the user issued `/compact`.
    async fn handle_compact(
        &mut self,
        command_text: String,
        sent_at: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<()> {
        // `compact_now` mints a turn-kind job, so its start + terminal
        // transitions drive the web chat's TurnState through the projector
        // — nothing to emit here.
        let text = self
            .volatile
            .agent_loop
            .compact_now(
                &mut self.durable.session,
                &self.volatile.job_lifecycle,
                &self.volatile.span_recorder,
                None,
                self.volatile.actor_token.child_token(),
            )
            .await?;
        // Record the `/compact` echo + its confirmation as out-of-band control
        // events (off the LLM transcript) so a reload shows them, anchored after
        // the (post-compaction) last row.
        if let Some(after) = self.current_after_ordinal().await {
            self.persist_control_event(after, ControlEventKind::Command, &command_text, sent_at)
                .await;
            self.persist_control_event(
                after,
                ControlEventKind::NoticeInfo,
                &text,
                chrono::Utc::now(),
            )
            .await;
        }
        let notice = AgentOutput {
            session_id: self.durable.session.id.clone(),
            user_id: self.durable.session.user.id.clone(),
            channel: self.durable.session.channel.clone(),
            event: AgentEvent::Notice {
                level: NoticeLevel::Info,
                text,
            },
        };
        self.send_response(notice, "compact").await;
        Ok(())
    }

    /// Run the agent loop for a subagent-spawned session. Distinct from
    /// `handle_user_input` because the JobInput must be `Spawned` (not
    /// `UserChat`) so `JobLifecycle::start_job`'s allowed-for check
    /// passes regardless of the inherited trigger kind.
    async fn handle_subagent_spawned(
        &mut self,
        incoming: IncomingMessage,
        parent_job_id: baybo_model::JobId,
    ) -> anyhow::Result<()> {
        let content = incoming.message.content;
        let response_tx = self.volatile.response_tx.clone();
        self.volatile
            .agent_loop
            .append_spawned_prompt(content.clone())
            .await?;
        let response = self
            .run_agent_loop(
                JobInput::Spawned {
                    initial_prompt: content,
                },
                Some(parent_job_id),
                Some(response_tx),
                None,
                None,
            )
            .await?;
        self.send_response(response.into(), "subagent").await;
        Ok(())
    }

    /// Single egress for actor-emitted `AgentOutput`. Just a thin
    /// wrapper around `response_tx.send(...).await` with a
    /// labelled-on-error log so the four call sites all funnel
    /// through the same warn format.
    async fn send_response(&self, output: AgentOutput, source: &str) {
        if let Err(e) = self.volatile.response_tx.send(output).await {
            warn!(error = %e, source, "failed to send agent output to channel");
        }
    }

    /// Append an out-of-band control event (slash-command echo / notice) to the
    /// session's control-event log — separate from the LLM transcript, surfaced
    /// only on the chat view. Best-effort: a write failure just means it won't
    /// reappear on reload.
    async fn persist_control_event(
        &self,
        after_ordinal: i64,
        kind: ControlEventKind,
        text: &str,
        at: chrono::DateTime<chrono::Utc>,
    ) {
        if let Err(e) = self
            .volatile
            .session_manager
            .append_control_event(&self.durable.session.id, after_ordinal, kind, text, at)
            .await
        {
            warn!(
                session_id = %self.durable.session.id,
                error = %e,
                "failed to persist control event"
            );
        }
    }

    /// The session's current last `session_messages.ordinal` (`-1` if none) — the
    /// anchor a control event records so the chat view interleaves it after that
    /// row even on scroll-up. `None` on a storage error: the caller then skips
    /// the write rather than mis-anchor it to the top of the oldest page.
    async fn current_after_ordinal(&self) -> Option<i64> {
        match self
            .volatile
            .session_manager
            .latest_session_ordinal(&self.durable.session.id)
            .await
        {
            Ok(max) => Some(max.unwrap_or(-1)),
            Err(e) => {
                warn!(
                    session_id = %self.durable.session.id,
                    error = %e,
                    "control event: latest-ordinal lookup failed; skipping persist"
                );
                None
            }
        }
    }
}

/// The plain text of a message's content blocks, space-joined — used to echo a
/// user's control command (`/stop`, `/compact`) into the control-event log.
pub(crate) fn slash_command_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.trim()),
            _ => None,
        })
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text(s.to_string())]
    }

    #[test]
    fn matches_bare_command() {
        assert!(is_compact_command(&text("/compact")));
        assert!(is_compact_command(&text("  /compact  ")));
    }

    #[test]
    fn is_case_insensitive_and_ignores_trailing_args() {
        assert!(is_compact_command(&text("/CompAct")));
        assert!(is_compact_command(&text("/compact whatever extra")));
    }

    #[test]
    fn rejects_other_inputs() {
        assert!(!is_compact_command(&text("compact")));
        assert!(!is_compact_command(&text("/compaction")));
        assert!(!is_compact_command(&text("see /compact below")));
        assert!(!is_compact_command(&text("")));
        assert!(!is_compact_command(&[]));
        assert!(!is_compact_command(&[ContentBlock::ToolResult {
            tool_use_id: "x".into(),
            content: "/compact".into(),
            meta: None,
        }]));
    }

    fn incoming(body: &str) -> IncomingMessage {
        use baybo_channels::Message;
        use baybo_model::{ChannelType, User};
        IncomingMessage {
            message: Message {
                id: "m".into(),
                session_id: "s".into(),
                channel: ChannelType::tui(),
                sender: User {
                    id: "u".into(),
                    name: None,
                    channel: ChannelType::tui(),
                },
                content: text(body),
                timestamp: chrono::Utc::now(),
                reply_to: None,
                metadata: Default::default(),
            },
            platform_msg_id: String::new(),
        }
    }

    fn user_input(body: &str) -> AgentMessage {
        AgentMessage::UserInput(Box::new(incoming(body)))
    }

    /// `MailboxInterjections` drains the leading run of non-slash `UserInput`s
    /// and stops at the first slash command, leaving it (and everything behind
    /// it) queued for the actor's normal dispatch.
    #[tokio::test]
    async fn mailbox_interjections_drain_non_slash_run_only() {
        use crate::runtime::agent_loop::InterjectionSource;

        let (tx, mut rx) = mailbox::channel::<AgentMessage>(16);
        tx.send(user_input("first")).await.unwrap();
        tx.send(user_input("second")).await.unwrap();
        tx.send(user_input("/compact")).await.unwrap();
        tx.send(user_input("third")).await.unwrap();

        let drained = {
            let mut src = MailboxInterjections { rx: &mut rx };
            src.drain_injectable()
        };
        assert_eq!(drained.len(), 2, "drains only the leading non-slash run");
        assert!(matches!(&drained[0][0], ContentBlock::Text(t) if t == "first"));
        assert!(matches!(&drained[1][0], ContentBlock::Text(t) if t == "second"));

        // The slash command stays at the head of the queue, undisturbed.
        match rx.try_recv() {
            Ok(AgentMessage::UserInput(m)) => {
                assert!(matches!(&m.message.content[0], ContentBlock::Text(t) if t == "/compact"));
            }
            other => panic!("expected the slash command still queued, got {other:?}"),
        }
    }

    /// `/stop` path: `discard_pending` drops the leading non-slash run without
    /// running it, and (like the drain) stops at the slash barrier — so a
    /// queued `/compact` and the message behind it survive for the next turn.
    #[tokio::test]
    async fn mailbox_interjections_discard_pending_drops_leading_run() {
        use crate::runtime::agent_loop::InterjectionSource;

        let (tx, mut rx) = mailbox::channel::<AgentMessage>(16);
        tx.send(user_input("first")).await.unwrap();
        tx.send(user_input("second")).await.unwrap();
        tx.send(user_input("/compact")).await.unwrap();
        tx.send(user_input("third")).await.unwrap();

        {
            let mut src = MailboxInterjections { rx: &mut rx };
            src.discard_pending();
        }

        // The two leading non-slash messages are gone; the slash barrier stays
        // at the head, the message behind it still queued for the next turn.
        match rx.try_recv() {
            Ok(AgentMessage::UserInput(m)) => {
                assert!(matches!(&m.message.content[0], ContentBlock::Text(t) if t == "/compact"));
            }
            other => panic!("expected the slash command still queued, got {other:?}"),
        }
        match rx.try_recv() {
            Ok(AgentMessage::UserInput(m)) => {
                assert!(matches!(&m.message.content[0], ContentBlock::Text(t) if t == "third"));
            }
            other => panic!("expected 'third' still queued behind the slash, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mailbox_interjections_discard_pending_drops_a_queued_batch() {
        use crate::runtime::agent_loop::InterjectionSource;

        // A coalesced `UserInputBatch` queued behind a stopped turn must ALSO be
        // dropped on discard — `is_coalescable_user_input` (mid-turn injection)
        // excludes it, so the discard path needs its own broader predicate.
        let (tx, mut rx) = mailbox::channel::<AgentMessage>(16);
        tx.send(user_input("first")).await.unwrap();
        tx.send(AgentMessage::UserInputBatch(vec![
            incoming("a"),
            incoming("b"),
        ]))
        .await
        .unwrap();
        tx.send(user_input("/compact")).await.unwrap();

        {
            let mut src = MailboxInterjections { rx: &mut rx };
            src.discard_pending();
        }

        // The leading `UserInput` and the `UserInputBatch` are gone; the slash
        // barrier stays at the head for the next turn.
        match rx.try_recv() {
            Ok(AgentMessage::UserInput(m)) => {
                assert!(matches!(&m.message.content[0], ContentBlock::Text(t) if t == "/compact"));
            }
            other => panic!("expected the slash command still queued, got {other:?}"),
        }
        assert!(
            rx.try_recv().is_err(),
            "nothing should remain after the slash barrier",
        );
    }

    /// A non-`UserInput` at the head is never injectable even at the same
    /// `Trigger` priority — the predicate keys off the variant, not the
    /// priority — so the drain yields nothing and leaves it queued.
    #[tokio::test]
    async fn mailbox_interjections_skip_when_top_is_not_user_input() {
        use crate::runtime::agent_loop::InterjectionSource;

        let (tx, mut rx) = mailbox::channel::<AgentMessage>(16);
        tx.send(AgentMessage::CronTrigger {
            job_id: "j".into(),
            prompt: "p".into(),
        })
        .await
        .unwrap();

        let drained = {
            let mut src = MailboxInterjections { rx: &mut rx };
            src.drain_injectable()
        };
        assert!(drained.is_empty());
        assert!(matches!(
            rx.try_recv(),
            Ok(AgentMessage::CronTrigger { .. })
        ));
    }
}
