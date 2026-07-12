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

use std::sync::Arc;

use crate::runtime::agent_loop::{InterjectionSource, UserInterjectionInput};
use baybo_channels::{
    AgentEvent, AgentOutput, COMPACT_COMMAND, IncomingMessage, NoticeLevel, OutgoingMessage,
};
use baybo_job::JobInput;
use baybo_model::{
    ContentBlock, ControlEventKind, ExecutionOutcome, LlmEntryName, PendingBackgroundResult,
    PendingCronResult, PendingNotificationTurn,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Hard cap on `session.state.pending_background_results`. A parent
/// that stays idle while many background subagents finish would
/// otherwise grow this vec without bound — both in memory and on
/// the persisted row. Once the cap is reached the oldest entry is
/// dropped (its content still lives in the child session's trace).
const MAX_PENDING_BACKGROUND_RESULTS: usize = 64;

/// Hard cap on `session.state.delivered_cron_executions` — the dedup keys of
/// one-shot results already appended here. Only a crash inside the
/// append→stamp window can replay a delivery, so a handful of recent keys is
/// all that is ever consulted; the cap keeps a long-lived conversation's row
/// from growing without bound.
const MAX_DELIVERED_CRON_EXECUTIONS: usize = 64;

/// How many rows to pull around a cron fire's reply ordinal when reading it out
/// of the fire's session. The reply sits exactly at the recorded ordinal; the
/// small margin tolerates an interleaved row without a second round-trip.
/// Mirrors the push dispatcher's preview read.
const FIRE_REPLY_READ_LIMIT: usize = 4;

/// How long after sealing a subagent group the barrier waits for all
/// members before firing partial + dissolving the cohort (still-running
/// members then deliver individually). Generous — group members are real
/// background subagents.
const GROUP_TIMEOUT_MINUTES: i64 = 30;

/// Exponential-backoff retry schedule for a FAILED `SubagentNotification`
/// turn. When the turn errors (provider / cost / cancel) and the session is
/// idle, the actor retries on this backoff so a fire-and-forget completion is
/// still reported during the idle window. Each step doubles, capped at
/// `NOTIFY_RETRY_MAX_BACKOFF`; an inbound message resets the schedule.
const NOTIFY_RETRY_INITIAL_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);
const NOTIFY_RETRY_MAX_BACKOFF: std::time::Duration = std::time::Duration::from_secs(300);

/// Delivery attempts for one notification ledger before the actor stops
/// retrying. The results are NOT lost at the cap: the prompt row is durable
/// in the transcript, so delivery degrades to *passive* — the next real
/// turn's model reads the results and reports then. The cap exists because
/// each retry's state persist bumps `last_active`, so a perpetually failing
/// turn would otherwise pin the actor resident forever.
const NOTIFY_TURN_MAX_ATTEMPTS: u32 = 5;

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
    /// A cron job fired. Runs in this (fresh, isolated) session; `delivery`
    /// decides where the reply goes. `title` names the job in the notification
    /// a non-reply outcome (failure, or a run that produced nothing) reports.
    CronTrigger {
        job_id: String,
        title: String,
        prompt: String,
        delivery: CronDelivery,
    },
    /// A one-shot cron fire finished, and its result belongs in **this**
    /// conversation (the one that scheduled the job). Handled at a turn
    /// boundary with **no inference**: the actor appends the framed result as
    /// an assistant row, dispatches it, and resolves the delivery ledger. See
    /// [`AgentActor::handle_cron_result_ready`].
    CronResultReady(Box<PendingCronResult>),
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

/// Where a cron fire's reply goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronDelivery {
    /// Out through this session's own channel — today's behavior, and what a
    /// **recurring** fire does: its session *is* the conversation the user
    /// reads the result in.
    Channel,
    /// Nowhere, from this session. A **one-shot** fire's session is transient
    /// and invisible; its result is delivered into the conversation that
    /// scheduled the job (as a `CronResultReady` there), so dispatching here
    /// too would notify the user twice.
    OriginSession,
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
            // Same tier as a finished background job: both are autonomous
            // deliveries that wait for a queued user turn but outrank a stop.
            AgentMessage::BackgroundJobFinished(_) | AgentMessage::CronResultReady(_) => {
                MessagePriority::BackgroundJobFinished
            }
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
    fn drain_injectable(&mut self) -> Vec<UserInterjectionInput> {
        let mut out = Vec::new();
        while let Some(AgentMessage::UserInput(inc)) =
            self.rx.try_recv_if(is_coalescable_user_input)
        {
            out.push(UserInterjectionInput {
                content: inc.message.content,
                platform_msg_id: inc.platform_msg_id,
            });
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

        // An undelivered notification (a buffered batch, an open barrier
        // cohort, or an open delivery ledger) is worked off on an exponential
        // backoff (capped at `NOTIFY_RETRY_MAX_BACKOFF`) rather than waiting
        // for the next inbound message, so a fire-and-forget completion is
        // still reported during the idle window. A real inbound message wins
        // the race (biased) and resets the backoff. Retries are capped at
        // `NOTIFY_TURN_MAX_ATTEMPTS`, after which delivery degrades to
        // passive (the prompt row is durable in the transcript).
        let mut notify_backoff = NOTIFY_RETRY_INITIAL_BACKOFF;
        loop {
            // Stay on the timed path while any notification work is
            // outstanding — including open barrier cohorts whose group
            // timeout must be enforced even with no inbound message.
            let next = if self.notification_work_outstanding() {
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
                        if self.notification_work_outstanding() {
                            notify_backoff = (notify_backoff * 2).min(NOTIFY_RETRY_MAX_BACKOFF);
                        } else {
                            notify_backoff = NOTIFY_RETRY_INITIAL_BACKOFF;
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
                    // Final state write so the durable row matches the actor's
                    // last in-memory state. Load-bearing for the notification
                    // ledger: if the post-delivery clear couldn't persist (a
                    // transient store error), the row still shows the ledger
                    // open — a rehydrated actor would re-run the turn and send
                    // a duplicate. This save heals that window.
                    self.persist_session_state_after_pending_change("actor_stop")
                        .await;
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
            AgentMessage::CronTrigger {
                job_id,
                title,
                prompt,
                delivery,
            } => {
                debug!(session_id = %session_id, job_id = %job_id, ?delivery, "received cron trigger");
                if let Err(e) = self
                    .dispatch_cron_prompt(&prompt, &job_id, &title, delivery)
                    .await
                {
                    error!(session_id = %session_id, job_id = %job_id, error = %e, "failed to handle cron trigger");
                    // A failed fire would otherwise leave a conversation that
                    // is empty when opened and never announced itself. Report
                    // it where the fire lives — but only for a fire that OWNS
                    // its conversation: a one-shot's failure is reported into
                    // the conversation that scheduled it, off this job's
                    // terminal lifecycle edge.
                    if matches!(delivery, CronDelivery::Channel) {
                        self.report_cron_outcome(&title, true, &e.to_string()).await;
                    }
                }
            }
            AgentMessage::CronResultReady(pending) => {
                self.handle_cron_result_ready(*pending).await;
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

    /// Dispatch a fired cron job through the agent loop.
    ///
    /// The cron fire mints a Cron-rooted session, so the job records
    /// `origin = Cron`. The content the LLM sees is framed + appended by
    /// `AgentLoop::append_cron_fire` (which uses `baybo_context::prompts::cron`)
    /// so the model treats it as a task to perform now rather than a live
    /// user message.
    ///
    /// `delivery` decides what happens to the reply. A recurring fire sends it
    /// out through this session's own channel — the session *is* the
    /// conversation the user reads. A one-shot's session is transient and
    /// invisible, so nothing goes out here: its result is picked up off this
    /// job's terminal lifecycle edge (by the router's cron waiter) and
    /// delivered into the conversation that scheduled it.
    async fn dispatch_cron_prompt(
        &mut self,
        prompt: &str,
        job_id: &str,
        title: &str,
        delivery: CronDelivery,
    ) -> anyhow::Result<()> {
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
        match delivery {
            CronDelivery::OriginSession => {}
            CronDelivery::Channel if is_blank_reply(&response.content) => {
                // The conversation IS the notification here, so a fire that
                // produced nothing must still say so. Suppressing the send
                // would leave a conversation that is empty when opened and —
                // because clients learn a new conversation exists from the
                // activity pulse the gateway derives from channel dispatch —
                // never announced itself at all.
                self.report_cron_outcome(title, false, "").await;
            }
            CronDelivery::Channel => {
                self.send_response(response.into(), "cron").await;
            }
        }
        Ok(())
    }

    /// Report a fire's non-reply outcome — a failure, or a run that produced
    /// nothing — in **its own** conversation, as the same framed
    /// `CronNotification` assistant row a one-shot delivers to its origin.
    ///
    /// A real row rather than a `Notice`, because the two are not equivalent
    /// where it counts: a row survives a reload, is read back by the model on a
    /// follow-up turn, raises the conversation's unread badge, and — riding a
    /// `CronNotification` job's `Completed { reply_ordinal }` edge — pushes to
    /// the user's phone. A notice does none of those. So every fire outcome
    /// produces exactly one notification row, whichever kind of fire it was.
    ///
    /// Only for a fire that owns its conversation (`CronDelivery::Channel`). A
    /// one-shot's outcome is reported into the conversation that *scheduled* it
    /// (see `handle_cron_result_ready`); reporting from its own invisible
    /// session as well would notify the user twice for one fire.
    async fn report_cron_outcome(&mut self, title: &str, failed: bool, detail: &str) {
        let content = vec![ContentBlock::Text(
            baybo_context::prompts::cron::frame_cron_notification(title, failed, detail),
        )];
        if let Err(e) = self.publish_cron_notification(content, None).await {
            warn!(
                session_id = %self.durable.session.id,
                error = %e,
                "failed to report the cron fire's outcome in its own conversation"
            );
        }
    }

    /// Append `content` as this session's cron-notification row, open a
    /// `CronNotification` job so its `Completed { reply_ordinal }` edge drives
    /// push off the durable row, and dispatch it to the channel. Returns the
    /// persisted ordinal.
    ///
    /// `remember` is the execution to record as delivered once the row is
    /// durable — the one-shot origin delivery's dedup key. `None` for a fire
    /// reporting in its own conversation, which has no cross-session ledger to
    /// keep.
    ///
    /// An append that does not reach the store fails the whole publish: the row
    /// would live only in this actor's memory, the push would have no durable
    /// row to preview, and a reload would show nothing. The caller decides what
    /// that means (the origin delivery leaves its ledger unresolved so the
    /// re-drive retries).
    async fn publish_cron_notification(
        &mut self,
        content: Vec<ContentBlock>,
        remember: Option<String>,
    ) -> anyhow::Result<i64> {
        let session_id = self.durable.session.id.clone();
        let job_lifecycle = Arc::clone(&self.volatile.job_lifecycle);
        let origin = self.durable.session.trigger.kind();
        let ordinal = crate::runtime::scope::with_job(
            &job_lifecycle,
            // Not a cancellable turn: the append is a single store write with
            // nothing to interrupt. The token is never tripped.
            CancellationToken::new(),
            crate::runtime::scope::JobSpec {
                session_id: session_id.clone(),
                origin,
                input: JobInput::CronNotification {
                    content: content.clone(),
                },
                parent_job_id: None,
            },
            |_job_id| async {
                let ordinal = self
                    .volatile
                    .agent_loop
                    .append_cron_notification(content.clone())
                    .await
                    .ok_or_else(|| {
                        anyhow::anyhow!("cron notification was not persisted to the transcript")
                    })?;
                // Record the dedup key as soon as the row IS durable, still
                // inside the job scope: everything after this point can fail
                // (the job's own `complete()` writes to the same store), and a
                // failure there must not leave an appended row whose execution
                // the re-drive would replay into a second copy.
                if let Some(execution_id) = remember.clone() {
                    self.remember_cron_delivery(execution_id).await;
                }
                Ok((
                    baybo_job::JobOutput::Message {
                        content: content.clone(),
                        ordinal: Some(ordinal),
                    },
                    ordinal,
                ))
            },
        )
        .await?;

        let out = AgentOutput {
            session_id: session_id.clone(),
            user_id: self.durable.session.user.id.clone(),
            channel: self.durable.session.channel.clone(),
            event: AgentEvent::Message(OutgoingMessage {
                session_id,
                user_id: self.durable.session.user.id.clone(),
                channel: self.durable.session.channel.clone(),
                content,
                reply_to: None,
                metadata: baybo_model::MessageMetadata::default(),
                ordinal: Some(ordinal),
            }),
        };
        self.send_response(out, "cron_notification").await;
        Ok(ordinal)
    }

    /// Deliver a finished one-shot cron fire's result into **this**
    /// conversation — the one that scheduled the job. Runs at a turn boundary
    /// with **no inference**: the fire already did the thinking, in its own
    /// isolated session, and re-deriving anything here would cost a second LLM
    /// call and let the model decide to stay quiet about a reminder the user
    /// asked for.
    ///
    /// Every outcome notifies, including failure and an empty reply
    /// (`build_cron_notification` frames each) — a scheduled task that
    /// silently evaporates is the one behaviour this feature must never have.
    async fn handle_cron_result_ready(&mut self, pending: PendingCronResult) {
        let session_id = self.durable.session.id.clone();
        if self
            .durable
            .session
            .state
            .delivered_cron_executions
            .contains(&pending.execution_id)
        {
            debug!(
                session_id = %session_id,
                execution_id = %pending.execution_id,
                "cron result already delivered to this session; ignoring replay"
            );
            // The append survived but the ledger stamp did not (that is the
            // only way a delivered result gets replayed). Stamp it now so the
            // boot re-drive stops re-routing it.
            self.resolve_cron_delivery(&pending.execution_id).await;
            return;
        }

        let content = self.build_cron_notification(&pending).await;
        // A notification landing is exactly what makes a hidden conversation
        // relevant again — and the push deep-links here, so the conversation
        // must be in the user's list when they tap it.
        self.unhide_for_cron_notification().await;

        if let Err(e) = self
            .publish_cron_notification(content, Some(pending.execution_id.clone()))
            .await
        {
            // Leave the ledger unresolved: the boot re-drive replays this
            // delivery rather than losing the user's reminder. If the row did
            // land before the failure, the dedup key landed with it, so the
            // replay is a no-op.
            error!(
                session_id = %session_id,
                execution_id = %pending.execution_id,
                error = %e,
                "cron result delivery failed; leaving it for re-drive"
            );
            return;
        }

        self.resolve_cron_delivery(&pending.execution_id).await;
        info!(
            session_id = %session_id,
            execution_id = %pending.execution_id,
            cron_job_id = %pending.cron_job_id,
            "delivered cron result to origin conversation"
        );
    }

    /// The notification's content: a header naming the job, the fire's own
    /// reply text (or a per-outcome fallback), and any media the fire
    /// produced.
    async fn build_cron_notification(&self, pending: &PendingCronResult) -> Vec<ContentBlock> {
        let failed = matches!(pending.outcome, ExecutionOutcome::Failed);
        let (body, attachments) = match pending.outcome {
            ExecutionOutcome::Failed => (
                pending.failure_reason.clone().unwrap_or_default(),
                Vec::new(),
            ),
            ExecutionOutcome::Blank => (String::new(), Vec::new()),
            ExecutionOutcome::Success => self.read_fire_reply(pending).await,
        };
        let header = baybo_context::prompts::cron::frame_cron_notification(
            &pending.job_title,
            failed,
            &body,
        );
        let mut content = vec![ContentBlock::Text(header)];
        content.extend(attachments);
        content
    }

    /// The fire's reply, read from its own session at the ordinal the job's
    /// `Completed` edge carried: its text (joined), plus any non-text blocks
    /// (images, files) so nothing the fire produced is dropped on the way
    /// over. An unreadable or absent row yields empty text, which
    /// `frame_cron_notification` turns into the blank-run fallback rather than
    /// a silent non-delivery.
    async fn read_fire_reply(&self, pending: &PendingCronResult) -> (String, Vec<ContentBlock>) {
        let Some(ordinal) = pending.reply_ordinal else {
            return (String::new(), Vec::new());
        };
        let rows = match self
            .volatile
            .session_manager
            .history_since(&pending.fire_session_id, ordinal - 1, FIRE_REPLY_READ_LIMIT)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                warn!(
                    fire_session_id = %pending.fire_session_id,
                    ordinal,
                    error = %e,
                    "failed to read cron fire reply; notifying without its body"
                );
                return (String::new(), Vec::new());
            }
        };
        let Some((_, _, reply)) = rows.into_iter().find(|(ord, _, _)| *ord == ordinal) else {
            warn!(
                fire_session_id = %pending.fire_session_id,
                ordinal,
                "cron fire reply row not found at its recorded ordinal"
            );
            return (String::new(), Vec::new());
        };

        let mut text = String::new();
        let mut attachments = Vec::new();
        for block in reply.content {
            match block {
                ContentBlock::Text(t) => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&t);
                }
                // Tool traffic and thinking are the fire's internals, not its
                // report; media it produced is part of the answer.
                ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. }
                | ContentBlock::Thinking { .. } => {}
                other => attachments.push(other),
            }
        }
        (text, attachments)
    }

    /// Un-hide this conversation so a delivered cron result can't land in a
    /// list the user removed it from.
    ///
    /// Written unconditionally rather than gated on `session.hidden`: that
    /// field is a snapshot taken when the actor was spawned, and `set_hidden`
    /// is a targeted column write that never notifies a live actor — so a user
    /// who hides the conversation while its actor is resident leaves the
    /// in-memory copy claiming `false`, and a gate would skip the un-hide for
    /// the one case it exists to cover. The store write is idempotent.
    async fn unhide_for_cron_notification(&mut self) {
        if let Err(e) = self
            .volatile
            .session_manager
            .set_hidden(&self.durable.session.id, false)
            .await
        {
            warn!(
                session_id = %self.durable.session.id,
                error = %e,
                "failed to un-hide conversation for cron notification"
            );
            return;
        }
        self.durable.session.hidden = false;
    }

    /// Record that this execution's result has landed in the transcript, and
    /// persist it — the session row is the only thing that survives an actor
    /// eviction, and it is what makes a replayed delivery a no-op.
    async fn remember_cron_delivery(&mut self, execution_id: String) {
        let delivered = &mut self.durable.session.state.delivered_cron_executions;
        if delivered.len() >= MAX_DELIVERED_CRON_EXECUTIONS {
            delivered.remove(0);
        }
        delivered.push(execution_id);
        self.persist_session_state_after_pending_change("cron_result_delivered")
            .await;
    }

    /// Stamp the execution's delivery as resolved. Failure is logged, not
    /// propagated: the result is already in the transcript, and an unresolved
    /// ledger only costs a replayed delivery at the next boot, which the dedup
    /// set absorbs.
    async fn resolve_cron_delivery(&self, execution_id: &str) {
        if let Err(e) = self
            .volatile
            .cron_store
            .mark_execution_notified(execution_id, chrono::Utc::now())
            .await
        {
            warn!(
                session_id = %self.durable.session.id,
                execution_id = %execution_id,
                error = %e,
                "failed to resolve cron delivery ledger; a boot re-drive will retry it"
            );
        }
    }

    async fn handle_user_input(&mut self, incoming: IncomingMessage) -> anyhow::Result<()> {
        let sent_at = incoming.message.timestamp;
        let platform_msg_id = incoming.platform_msg_id;
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
            .append_user_message_with_platform_msg_id(content.clone(), platform_msg_id)
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
                .append_user_message_with_platform_msg_id(
                    incoming.message.content,
                    incoming.platform_msg_id,
                )
                .await?;
        }
        self.volatile
            .agent_loop
            .append_user_message_with_platform_msg_id(last.message.content, last.platform_msg_id)
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
    /// session store so the latest `pending_background_results` /
    /// notification ledger survives eviction. Called only by the
    /// background-subagent paths today. Logs a warn on storage error rather
    /// than failing the surrounding handler — losing the persisted copy
    /// degrades to "delivered to the live actor only" rather than "delivered
    /// nowhere", which is strictly worse than the v1 (mailbox-only) baseline.
    /// Updates `last_active` in passing so the reaper doesn't immediately
    /// re-target the actor after the counter clears.
    ///
    /// Returns whether the save landed. Most callers can ignore it (the
    /// in-memory copy is authoritative for the live actor); the notification
    /// drain gates on it so a turn never runs ahead of a ledger the store
    /// doesn't have.
    async fn persist_session_state_after_pending_change(&mut self, context: &'static str) -> bool {
        self.durable.session.last_active = chrono::Utc::now();
        match self
            .volatile
            .session_manager
            .store()
            .save(&self.durable.session)
            .await
        {
            Ok(()) => true,
            Err(e) => {
                warn!(
                    session_id = %self.durable.session.id,
                    context = %context,
                    error = %e,
                    "failed to persist session.state after pending-subagent change; live actor still has the latest copy in memory"
                );
                false
            }
        }
    }

    /// Any undelivered notification work: a buffered batch, an open barrier
    /// cohort (its timeout must be enforced), or an open delivery ledger (a
    /// persisted prompt row awaiting its reply). One predicate for the run
    /// loop's stay-on-timed-path decision AND its backoff reset-vs-double
    /// decision — the two must agree, or a provider outage retries at the
    /// initial cadence forever instead of settling at the cap.
    fn notification_work_outstanding(&self) -> bool {
        !self
            .durable
            .session
            .state
            .pending_background_results
            .is_empty()
            || self.has_open_groups()
            || self
                .durable
                .session
                .state
                .pending_notification_turn
                .is_some()
    }

    /// Surface buffered background-subagent results as their own turn —
    /// but only when no higher-priority work is queued: a `Trigger`
    /// (UserInput) must run first, and another queued `BackgroundJobFinished`
    /// is folded in first (merge). An empty queue or a lowest-priority
    /// `ActorStop` means "drain now".
    ///
    /// An open delivery ledger defers to the run loop's timed backoff arm
    /// instead: retrying here would fire a proactive re-turn immediately
    /// behind every inbound message (whose request already carried the
    /// persisted prompt row).
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
            || self
                .durable
                .session
                .state
                .pending_notification_turn
                .is_some()
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
    ///
    /// Delivery is ledgered: the prompt row is persisted to the transcript
    /// up front and recorded on `session.state.pending_notification_turn`,
    /// then the turn runs with **no rollback of any kind** — a failed
    /// attempt's partial rows stay (they are real history the prompt row
    /// explains), and the ledger drives a forward-only retry. Crash stance:
    /// a crash mid-turn re-runs the turn on rehydration — a duplicate report
    /// beats a lost one, the same direction the cron delivery ledger chose.
    async fn run_subagent_notification(&mut self) {
        // An open ledger means a prior batch's prompt row is already in the
        // transcript without a delivered reply. Finish that first; a fresh
        // batch stays buffered as the next one.
        if self
            .durable
            .session
            .state
            .pending_notification_turn
            .is_some()
        {
            self.retry_notification_turn().await;
            return;
        }
        // Take the batch. The actor is single-threaded — nothing else
        // mutates the buffer while the drain runs.
        let pending = std::mem::take(&mut self.durable.session.state.pending_background_results);
        if pending.is_empty() {
            return;
        }
        let content = baybo_context::prompts::subagent::build_notification_content(&pending);
        // Persist the prompt row FIRST. From here the results are durable in
        // the transcript itself — even a crashed actor's next real turn reads
        // them — so the buffer can empty and no path needs to restore it.
        let Some(prompt_ordinal) = self
            .volatile
            .agent_loop
            .append_subagent_notification(content.clone())
            .await
        else {
            // Persist failed (the window was popped back in lockstep). Put
            // the batch back untouched; the backoff retries the whole drain.
            warn!(
                session_id = %self.durable.session.id,
                "notification prompt row failed to persist; re-buffering the batch for retry"
            );
            self.durable.session.state.pending_background_results = pending;
            return;
        };
        self.durable.session.state.pending_notification_turn = Some(PendingNotificationTurn {
            content: content.clone(),
            prompt_ordinal,
            attempts: 0,
        });
        // One durable commit: buffer emptied + ledger opened. If it fails,
        // don't run the turn — the in-memory ledger stays set, so the next
        // timed-arm tick lands in the retry branch, which re-persists before
        // running. (A crash in this window re-drives from the still-buffered
        // batch and duplicates one prompt row: duplicate-beats-lost.)
        if !self
            .persist_session_state_after_pending_change("subagent_notification_drained")
            .await
        {
            return;
        }
        self.drive_notification_turn(content).await;
    }

    /// Re-drive an open delivery ledger: the prompt row is persisted but no
    /// reply was delivered. Forward-only — nothing is rolled back; a cue row
    /// re-anchors the request tail and the same turn re-runs.
    async fn retry_notification_turn(&mut self) {
        // The ledger (or its last attempts bump) may never have reached the
        // store. Re-persist before running so a crash can't lose the
        // bookkeeping the turn is about to depend on.
        if !self
            .persist_session_state_after_pending_change("subagent_notification_retry")
            .await
        {
            return;
        }
        let Some(ledger) = self.durable.session.state.pending_notification_turn.clone() else {
            return;
        };
        // Compaction supersedes every active row and re-inserts the kept
        // slice at fresh ordinals, so the recorded ordinal dangles after any
        // compaction — even when the prompt's content survived verbatim. The
        // ledger froze the content, so the repair is a re-append.
        let prompt_active = match self
            .volatile
            .session_manager
            .load_session_messages_with_supersede(&self.durable.session.id)
            .await
        {
            Ok(rows) => rows
                .iter()
                .any(|r| r.ordinal == ledger.prompt_ordinal && r.superseded_by.is_none()),
            Err(e) => {
                warn!(
                    session_id = %self.durable.session.id,
                    error = %e,
                    "could not verify the notification prompt row; retrying next tick"
                );
                return;
            }
        };
        if prompt_active {
            // Cue row (persisted): restores a user-side request tail — a
            // cancelled attempt's salvage leaves an assistant row at the
            // tail, and a request ending on an assistant message is provider
            // prefill (Anthropic rejects it outright with extended thinking
            // on) — un-buries the prompt from behind the failed attempt's
            // partial rows, and makes a blank reply a genuine judgment.
            let cue = baybo_context::prompts::subagent::build_retry_cue(
                ledger.attempts.saturating_add(1),
            );
            if self
                .volatile
                .agent_loop
                .append_subagent_notification(cue)
                .await
                .is_none()
            {
                return;
            }
        } else {
            let Some(new_ordinal) = self
                .volatile
                .agent_loop
                .append_subagent_notification(ledger.content.clone())
                .await
            else {
                return;
            };
            if let Some(l) = self
                .durable
                .session
                .state
                .pending_notification_turn
                .as_mut()
            {
                l.prompt_ordinal = new_ordinal;
            }
            self.persist_session_state_after_pending_change("subagent_notification_reanchored")
                .await;
        }
        self.drive_notification_turn(ledger.content).await;
    }

    /// Run the notification turn against the (already persisted) prompt and
    /// settle the ledger: clear on success, bump attempts on failure, degrade
    /// to passive delivery at the cap.
    async fn drive_notification_turn(&mut self, content: Vec<ContentBlock>) {
        // No delta streaming: the empty-output decision is made on the
        // assembled reply, so nothing may have been streamed already.
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
                let attempts = self
                    .durable
                    .session
                    .state
                    .pending_notification_turn
                    .as_ref()
                    .map(|l| l.attempts)
                    .unwrap_or(0);
                self.durable.session.state.pending_notification_turn = None;
                // If this clear-persist fails, the in-memory ledger is
                // already None and any later successful save (including the
                // ActorStop final write) heals the row — so a reaped actor
                // can't resurrect a delivered ledger into a duplicate send.
                self.persist_session_state_after_pending_change("subagent_notification_delivered")
                    .await;
                if is_blank_reply(&response.content) {
                    if attempts > 0 {
                        // With the retry cue in the request, a blank reply is
                        // a real judgment — but after failures it may also be
                        // a wedge; keep it observable.
                        warn!(
                            session_id = %self.durable.session.id,
                            attempts,
                            "subagent-notification retry produced no output; suppressing send"
                        );
                    } else {
                        debug!(
                            session_id = %self.durable.session.id,
                            "subagent-notification produced no output; suppressing send"
                        );
                    }
                    return;
                }
                self.send_response(response.into(), "subagent_notification")
                    .await;
            }
            Err(e) => {
                let attempts = match self
                    .durable
                    .session
                    .state
                    .pending_notification_turn
                    .as_mut()
                {
                    Some(ledger) => {
                        ledger.attempts = ledger.attempts.saturating_add(1);
                        ledger.attempts
                    }
                    None => 0,
                };
                if attempts >= NOTIFY_TURN_MAX_ATTEMPTS {
                    warn!(
                        session_id = %self.durable.session.id,
                        attempts,
                        error = %e,
                        "subagent-notification turn kept failing; ceasing active retries — \
                         the results are persisted in the transcript, so the next real turn \
                         reports them (passive delivery)"
                    );
                    self.durable.session.state.pending_notification_turn = None;
                } else {
                    error!(
                        session_id = %self.durable.session.id,
                        attempts,
                        error = %e,
                        "subagent-notification turn failed; the delivery ledger will retry"
                    );
                }
                self.persist_session_state_after_pending_change("subagent_notification_failed")
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
        // `compact_now` mints its own maintenance job and returns a control
        // notice; it does not emit a chat reply from the actor itself.
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
        assert!(matches!(&drained[0].content[0], ContentBlock::Text(t) if t == "first"));
        assert!(matches!(&drained[1].content[0], ContentBlock::Text(t) if t == "second"));

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
            title: "t".into(),
            prompt: "p".into(),
            delivery: CronDelivery::Channel,
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
