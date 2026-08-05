//! Actor model + supervision: one `AgentActor` per session, supervised
//! by [`AgentSupervisor`](crate::actor::supervisor::AgentSupervisor),
//! routed to by [`Router`](crate::actor::router::Router), and
//! checkpointed via [`DurableActorState`](crate::actor::state::DurableActorState).

mod background_notification;
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
use baybo_model::{
    ContentBlock, ControlEventKind, ExecutionOutcome, LlmEntryName, PendingBackgroundResult,
    PendingCronResult,
};
use baybo_session::SessionMessageAppendOutcome;
use baybo_turn::TurnInput;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

const CRON_NOTIFICATION_SOURCE_EVENT_PREFIX: &str = "cron-execution:";

/// How many rows to pull around a cron fire's reply ordinal when reading it out
/// of the fire's session. The reply sits exactly at the recorded ordinal; the
/// small margin tolerates an interleaved row without a second round-trip.
/// Mirrors the push dispatcher's preview read.
const FIRE_REPLY_READ_LIMIT: usize = 4;

/// Surfaced (as a `Notice`) when a *user* turn yields a blank reply — the
/// user is waiting, so acknowledge rather than push an empty bubble. Non-user
/// turns (cron, background notification) silently suppress a blank reply.
const EMPTY_USER_REPLY_NOTICE: &str =
    "The assistant did not produce a response to your message. Please try again or rephrase.";

use crate::actor::state::{DurableActorState, VolatileResources};
use crate::actor::supervisor::ActorRegistryGuard;

/// Attached (via anyhow context) to the error a turn returns when its
/// cancellation token tripped — a user `/stop` or actor shutdown, which is
/// normal operation, not a failure. The mailbox log sites downcast on it to
/// log the outcome at info instead of error, keeping ERROR a page-worthy
/// signal. `run_agent_loop` is the single choke point that tags it, since the
/// token discriminator only exists there.
#[derive(Debug, thiserror::Error)]
#[error("turn cancelled")]
pub(crate) struct TurnCancelled;

/// True when `err` carries the [`TurnCancelled`] marker anywhere in its
/// context chain — i.e. the turn ended via `/stop` or shutdown, not a failure.
fn is_turn_cancelled(err: &anyhow::Error) -> bool {
    err.downcast_ref::<TurnCancelled>().is_some()
}

/// What a **built-in** cron fire carries beyond an ordinary one: material
/// the runtime computed at fire time.
///
/// A struct rather than a bare `Option<String>` because it is the named seam
/// a second runtime-owned job extends — the dream pass is only its first
/// user, and what one of these fires needs to be told is not going to stay
/// one string forever.
#[derive(Debug, Clone, Default)]
pub struct BuiltinFireContext {
    /// Spliced into the framed prompt ahead of the job's instruction, so
    /// `original_cron_prompt` still recovers the instruction as configured.
    pub prompt_context: String,
}

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
    /// decides where the reply goes. `title` names the turn in the notification
    /// a non-reply outcome (failure, or a run that produced nothing) reports.
    CronTrigger {
        job_id: String,
        title: String,
        prompt: String,
        delivery: CronDelivery,
        /// Present only for a runtime-owned job's fire — see
        /// [`BuiltinFireContext`]. `None` for every job a user created.
        builtin: Option<BuiltinFireContext>,
    },
    /// One execution of a board issue. Runs in the issue's own session, so
    /// a follow-up run sees what the last one did. Nothing is dispatched to
    /// a channel: the outcome is reported onto the card by the waiter that
    /// watches this turn's terminal edge.
    IssueRun {
        run_id: baybo_model::IssueRunId,
        number: i64,
        brief: String,
        /// The worktree cut for this issue. Carried to the model because
        /// the Bash tool's description is rendered once per process and
        /// names the workspace work dir — nothing else would tell the run
        /// where it actually is.
        checkout: String,
    },
    /// A one-shot cron fire finished, and its result belongs in **this**
    /// conversation (the one that scheduled the turn). Handled at a turn
    /// boundary with **no inference**: the actor appends the framed result as
    /// an assistant row, dispatches it, and resolves the delivery ledger. See
    /// [`AgentActor::handle_cron_result_ready`].
    CronResultReady(Box<PendingCronResult>),
    /// A subagent was spawned. Carries the initial prompt assembled by
    /// `Router::handle_subagent_spawn` and the parent's `TurnId` for
    /// lineage. The child actor runs `agent_loop.run` with `TurnInput::Spawned`;
    /// the turn records the child session's root trigger as its `origin`
    /// (subagents inherit the parent's trigger — cron / system — via
    /// `create_spawned_session`), with no payload/trigger pairing constraint.
    SubagentSpawned {
        initial_message: Box<IncomingMessage>,
        parent_turn_id: baybo_model::TurnId,
    },
    /// A detached subagent or `Bash` command reached a terminal state. The
    /// completion enters `session.state.background_notifications`, where it
    /// is grouped/buffered and later delivered by one autonomous notification
    /// turn after higher-priority work drains.
    BackgroundJobFinished(Box<PendingBackgroundResult>),
    /// Re-pin the session's LLM (chat per-session model switch). `llm`
    /// is the `baybo.json` entry name to resolve against, or `None` to
    /// revert to `default-llm`. The handler re-pins the live loop in
    /// place and persists `session.state.last_llm` so the choice
    /// survives eviction. Processed at a turn boundary (the mailbox is
    /// drained sequentially), so it never swaps the model mid-turn —
    /// it takes effect on the next turn. Routed by the gateway's
    /// `PUT /v1/chat/sessions/{id}/model` via
    /// [`supervisor::AgentSupervisor::route`].
    SetModel {
        llm: Option<LlmEntryName>,
        /// The model within `llm`'s entry (a `model_list` id), or
        /// `None` for the entry's default. Persisted to
        /// `session.state.last_model` alongside `last_llm`.
        model: Option<String>,
        /// Per-session reasoning effort, or `None` for the entry default.
        /// Persisted to `session.state.last_effort`.
        effort: Option<String>,
    },
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
    /// scheduled the turn (as a `CronResultReady` there), so dispatching here
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
            | AgentMessage::IssueRun { .. }
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

/// A background-notification reply is suppressed when it carries no
/// non-whitespace text — the model's implicit way to stay quiet.
fn is_blank_reply(content: &[ContentBlock]) -> bool {
    content.iter().all(|b| match b {
        ContentBlock::Text(t) => t.trim().is_empty(),
        _ => false,
    })
}

fn cron_notification_source_event_id(execution_id: &str) -> String {
    format!("{CRON_NOTIFICATION_SOURCE_EVENT_PREFIX}{execution_id}")
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

        // Notification work uses a timer so a fire-and-forget result can be
        // reported while the session is otherwise idle. Inbound messages win
        // the biased race and reset the retry schedule.
        let mut notification_retry =
            background_notification::BackgroundNotificationRetrySchedule::new();
        loop {
            let next = if self.has_background_notification_work() {
                tokio::select! {
                    biased;
                    m = mailbox.recv() => m,
                    _ = tokio::time::sleep(notification_retry.delay()) => {
                        self.handle_background_notification_timer().await;
                        notification_retry.after_timer_attempt(
                            self.has_background_notification_work(),
                        );
                        continue;
                    }
                }
            } else {
                mailbox.recv().await
            };
            let Some(msg) = next else {
                break;
            };
            notification_retry.reset();
            match msg {
                AgentMessage::ActorStop => {
                    debug!(session_id = %session_id, "actor stopping");
                    // Final state write so the durable row matches the actor's
                    // last in-memory state. Load-bearing for the notification
                    // ledger: if the post-delivery clear couldn't persist (a
                    // transient store error), the row still shows the ledger
                    // open — a rehydrated actor would re-run the turn and send
                    // a duplicate. This save heals that window. Activity is
                    // preserved: reaped-idle sessions must not jump to the top
                    // of the recency-ordered chat list.
                    self.persist_session_state_preserving_activity("actor_stop")
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
                        if is_turn_cancelled(&e) {
                            info!(session_id = %session_id, "turn cancelled");
                        } else {
                            error!(
                                session_id = %session_id,
                                error = %e,
                                "failed to handle user input"
                            );
                        }
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
                        if is_turn_cancelled(&e) {
                            info!(session_id = %session_id, "turn cancelled");
                        } else {
                            error!(
                                session_id = %session_id,
                                error = %e,
                                "failed to handle user input batch"
                            );
                        }
                    }
                }
                other => {
                    self.dispatch_one(other).await;
                }
            }
            // The turn that just ran finished dispatching its grouped spawns,
            // so seal their cohorts: membership is now final and the barrier
            // (and its timeout) can fire.
            self.seal_background_notification_groups().await;
            // Surface buffered background-turn results as their own turn once
            // nothing higher-priority remains queued.
            self.maybe_start_background_notification(&mailbox).await;
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
                    if is_turn_cancelled(&e) {
                        info!(session_id = %session_id, "turn cancelled");
                    } else {
                        error!(session_id = %session_id, error = %e, "failed to handle user input");
                    }
                }
            }
            AgentMessage::IssueRun {
                run_id,
                number,
                brief,
                checkout,
            } => {
                debug!(session_id = %session_id, %run_id, number, "received issue run");
                if let Err(e) = self
                    .dispatch_issue_run(&run_id, number, &brief, &checkout)
                    .await
                {
                    // A cancelled run is not a failed one — the operator
                    // asked for it to stop, and the waiter records that.
                    if is_turn_cancelled(&e) {
                        info!(session_id = %session_id, %run_id, "issue run cancelled");
                    } else {
                        error!(session_id = %session_id, %run_id, error = %e, "issue run failed");
                    }
                }
            }
            AgentMessage::CronTrigger {
                job_id,
                title,
                prompt,
                delivery,
                builtin,
            } => {
                debug!(session_id = %session_id, job_id = %job_id, ?delivery, "received cron trigger");
                if let Err(e) = self
                    .dispatch_cron_prompt(&prompt, &job_id, &title, delivery, builtin)
                    .await
                {
                    if is_turn_cancelled(&e) {
                        // A cancelled fire (shutdown / `/stop`) is not a failed
                        // fire: log it as such and announce nothing.
                        info!(session_id = %session_id, job_id = %job_id, "turn cancelled");
                    } else {
                        error!(session_id = %session_id, job_id = %job_id, error = %e, "failed to handle cron trigger");
                        // A failed fire would otherwise leave a conversation that
                        // is empty when opened and never announced itself. Report
                        // it where the fire lives — but only for a fire that OWNS
                        // its conversation: a one-shot's failure is reported into
                        // the conversation that scheduled it, off this turn's
                        // terminal lifecycle edge.
                        if matches!(delivery, CronDelivery::Channel) {
                            self.report_cron_outcome(&title, true, &e.to_string()).await;
                        }
                    }
                }
            }
            AgentMessage::CronResultReady(pending) => {
                self.handle_cron_result_ready(*pending).await;
            }
            AgentMessage::SubagentSpawned {
                initial_message,
                parent_turn_id,
            } => {
                if let Err(e) = self
                    .handle_subagent_spawned(*initial_message, parent_turn_id)
                    .await
                {
                    if is_turn_cancelled(&e) {
                        info!(session_id = %session_id, "turn cancelled");
                    } else {
                        error!(session_id = %session_id, error = %e, "failed to handle subagent spawn");
                    }
                }
            }
            AgentMessage::BackgroundJobFinished(pending) => {
                self.handle_background_finished(*pending).await;
            }
            AgentMessage::SetModel { llm, model, effort } => {
                self.handle_set_model(llm, model, effort);
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
    /// `TurnLifecycle` itself on the broadcast bus
    /// (`subscribe_lifecycle_events`); the actor no longer emits a
    /// piggy-back signal on the response channel. Used by every
    /// handler that delegates turn lifecycle to `agent_loop.run`
    /// (UserInput, SubagentSpawned, cron prompt dispatch). Returns
    /// the loop's response on `Ok`; caller is responsible for sending
    /// it to the response channel.
    /// Run the loop on the session's *current* context. The triggering
    /// message must already be appended (the callers do this via
    /// `AgentLoop::append_user_message` / `append_cron_fire` /
    /// `append_background_notification_prompt_once`) so framing lives in
    /// `baybo-context` and the loop just iterates.
    async fn run_agent_loop(
        &mut self,
        turn_input: TurnInput,
        parent_turn_id: Option<baybo_model::TurnId>,
        delta_tx: Option<mpsc::Sender<AgentOutput>>,
        interjections: Option<&mut dyn crate::runtime::agent_loop::InterjectionSource>,
        // Set to whether this turn ended via cancellation (`/stop` / shutdown),
        // so the user-turn caller can drop queued interjections a `/stop` would
        // otherwise leave to run as follow-up turns. Caught after the loop fully
        // returns, so it covers every cancellation path (incl. mid-LLM-call).
        stopped_out: Option<&mut bool>,
        // A recurring cron fire's silence handle for `report_nothing`;
        // `None` for every other turn. See `dispatch_cron_prompt`.
        notify_silence: Option<baybo_tools::NotifySilence>,
    ) -> anyhow::Result<OutgoingMessage> {
        let is_user_turn = matches!(turn_input.input_kind(), baybo_turn::TurnInputKind::UserChat);
        // Kept so the error path below can tell a user `/stop` (token
        // tripped via the turn cancel) apart from a genuine failure.
        let turn_token = self.volatile.actor_token.child_token();
        // The actor emits no `TurnState`: both edges are projected from the
        // turn store by `spawn_turn_state_projector` — the start edge from
        // this turn's turn `start()` (`Pending → InProgress`) inside
        // `agent_loop.run`, the end edge from its terminal transition. So
        // chat turn-activity has a single producer sourced from the one
        // truth, and the per-`Subscribe` snapshot can't disagree with it.
        let result = self
            .volatile
            .agent_loop
            .run(
                &mut self.durable.session,
                turn_input,
                &self.volatile.turn_lifecycle,
                &self.volatile.span_recorder,
                parent_turn_id,
                delta_tx,
                turn_token.clone(),
                interjections,
                notify_silence,
            )
            .await;
        let cancelled = turn_token.is_cancelled();
        if let Some(out) = stopped_out {
            *out = cancelled;
        }
        // A user is waiting on this turn — a genuine failure must surface as
        // a terminal notice, not silence (the log line alone leaves the chat
        // dangling on its last progress frame). Cancellation stays quiet:
        // `/stop` already acknowledged with its own notice. Non-user turns
        // keep their own policies (cron logs, background-notification retries).
        if is_user_turn
            && !cancelled
            && let Err(e) = &result
        {
            let notice = AgentOutput {
                session_id: self.durable.session.id.clone(),
                user_id: self.durable.session.user.id.clone(),
                channel: self.durable.session.channel.clone(),
                event: AgentEvent::Notice {
                    level: NoticeLevel::Error,
                    text: format!("The turn failed before producing a reply: {e}"),
                    mid_turn: false,
                    durable_id: None,
                },
            };
            self.send_response(notice, "user_turn_failed").await;
        }
        // Tag a cancelled turn's error so the mailbox log sites classify a
        // user `/stop` (or shutdown) as info, not a page-worthy error. The
        // token check is the one discriminator that also covers the untyped
        // cancellation errors on this path (agent-loop-cancelled, and the
        // `scope.rs` turn-cancelled / already-terminal strings).
        match result {
            Err(e) if cancelled => Err(e.context(TurnCancelled)),
            other => other,
        }
    }

    /// Dispatch a fired cron job through the agent loop.
    ///
    /// The cron fire mints a Cron-rooted session, so the turn records
    /// `origin = Cron`. The content the LLM sees is framed + appended by
    /// `AgentLoop::append_cron_fire` (which uses `baybo_context::prompts::cron`)
    /// so the model treats it as a task to perform now rather than a live
    /// user message.
    ///
    /// `delivery` decides what happens to the reply. A recurring fire sends it
    /// out through this session's own channel — the session *is* the
    /// conversation the user reads. A one-shot's session is transient and
    /// invisible, so nothing goes out here: its result is picked up off this
    /// turn's terminal lifecycle edge (by the router's cron waiter) and
    /// delivered into the conversation that scheduled it.
    /// Run an issue's brief as one turn. No channel dispatch: an issue's
    /// audience is its card, and the run's outcome is recorded by the
    /// waiter off this turn's terminal lifecycle edge.
    async fn dispatch_issue_run(
        &mut self,
        run_id: &baybo_model::IssueRunId,
        number: i64,
        brief: &str,
        checkout: &str,
    ) -> anyhow::Result<()> {
        let turn_input = TurnInput::IssueRun {
            run_id: run_id.clone(),
            brief: vec![baybo_model::ContentBlock::Text(brief.to_string())],
        };
        self.volatile
            .agent_loop
            .append_issue_brief(number, checkout, brief)
            .await?;
        self.run_agent_loop(turn_input, None, None, None, None, None)
            .await?;
        Ok(())
    }

    async fn dispatch_cron_prompt(
        &mut self,
        prompt: &str,
        job_id: &str,
        title: &str,
        delivery: CronDelivery,
        builtin: Option<BuiltinFireContext>,
    ) -> anyhow::Result<()> {
        let turn_input = TurnInput::Cron {
            action_payload: serde_json::json!({
                "cron_job_id": job_id,
                "prompt": prompt,
            }),
        };
        let prompt_context = builtin.map(|ctx| ctx.prompt_context);
        self.volatile
            .agent_loop
            .append_cron_fire(job_id, prompt, prompt_context.as_deref())
            .await?;
        // Only a recurring fire (which owns a listed conversation) can be
        // silenced by `report_nothing`; a one-shot always reports back, so it
        // gets no handle and the tool is never even offered in its session.
        let silence =
            matches!(delivery, CronDelivery::Channel).then(baybo_tools::NotifySilence::new);
        let response = self
            .run_agent_loop(turn_input, None, None, None, None, silence.clone())
            .await?;
        let silenced = silence.as_ref().is_some_and(|s| s.requested());
        match delivery {
            CronDelivery::OriginSession => {}
            CronDelivery::Channel if silenced => {
                // `report_nothing`: this fire notifies no one. Push was already
                // skipped (its turn completed with no reply ordinal) and we
                // dispatch nothing here, so no activity pulse fires; hide the
                // fire's own conversation so it leaves no empty row in the list.
                // The reply row and transcript survive — nothing is deleted.
                self.hide_silenced_cron_fire().await;
            }
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

    /// A recurring fire that called `report_nothing`: drop it from the user's
    /// chat list, leaving no empty conversation behind. The inverse of
    /// `unhide_for_cron_notification`'s targeted write — the fire's reply row
    /// and transcript stay in the store, recoverable and inspectable
    /// (`include_cron`); only its listing is removed.
    async fn hide_silenced_cron_fire(&mut self) {
        if let Err(e) = self
            .volatile
            .session_manager
            .set_hidden(&self.durable.session.id, true)
            .await
        {
            warn!(
                session_id = %self.durable.session.id,
                error = %e,
                "failed to hide a silenced cron fire"
            );
            return;
        }
        self.durable.session.hidden = true;
    }

    /// Report a fire's non-reply outcome — a failure, or a run that produced
    /// nothing — in **its own** conversation, as the same framed
    /// `CronNotification` assistant row a one-shot delivers to its origin.
    ///
    /// A real row rather than a `Notice`, because the two are not equivalent
    /// where it counts: a row survives a reload, is read back by the model on a
    /// follow-up turn, raises the conversation's unread badge, and — riding a
    /// `CronNotification` turn's `Completed { reply_ordinal }` edge — pushes to
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
    /// `CronNotification` turn so its `Completed { reply_ordinal }` edge drives
    /// push off the durable row, and dispatch it to the channel. Returns the
    /// persisted ordinal.
    ///
    /// `source_event_id` makes a one-shot origin append idempotent. `None` is
    /// used by a recurring fire reporting in its own conversation, where each
    /// fire has a fresh session.
    ///
    /// An append that does not reach the store fails the whole publish: the row
    /// would live only in this actor's memory, the push would have no durable
    /// row to preview, and a reload would show nothing. The caller decides what
    /// that means (the origin delivery leaves its ledger unresolved so the
    /// re-drive retries).
    async fn publish_cron_notification(
        &mut self,
        content: Vec<ContentBlock>,
        source_event_id: Option<String>,
    ) -> anyhow::Result<SessionMessageAppendOutcome> {
        let session_id = self.durable.session.id.clone();
        if let Some(source_event_id) = source_event_id.as_deref()
            && let Some(ordinal) = self
                .volatile
                .session_manager
                .find_message_ordinal_by_source_event_id(&session_id, source_event_id)
                .await?
        {
            return Ok(SessionMessageAppendOutcome::Existing { ordinal });
        }

        let turn_lifecycle = Arc::clone(&self.volatile.turn_lifecycle);
        let origin = self.durable.session.trigger.kind();
        let append_outcome = crate::runtime::scope::with_turn(
            &turn_lifecycle,
            // Not a cancellable turn: the append is one store write with
            // nothing to interrupt. The token is never tripped.
            CancellationToken::new(),
            crate::runtime::scope::TurnSpec {
                session_id: session_id.clone(),
                origin,
                input: TurnInput::CronNotification {
                    content: content.clone(),
                },
                parent_turn_id: None,
            },
            |_turn_id| async {
                let append_outcome = self
                    .volatile
                    .agent_loop
                    .append_cron_notification(content.clone(), source_event_id.as_deref())
                    .await
                    .ok_or_else(|| {
                        anyhow::anyhow!("cron notification was not persisted to the transcript")
                    })?;
                let SessionMessageAppendOutcome::Inserted { ordinal } = append_outcome else {
                    return Err(anyhow::anyhow!(
                        "cron notification source event was inserted concurrently"
                    ));
                };
                Ok((
                    baybo_turn::TurnOutput::Message {
                        content: content.clone(),
                        ordinal: Some(ordinal),
                    },
                    append_outcome,
                ))
            },
        )
        .await?;
        let ordinal = append_outcome.ordinal();

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
        Ok(append_outcome)
    }

    /// Deliver a finished one-shot cron fire's result into **this**
    /// conversation — the one that scheduled the turn. Runs at a turn boundary
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
        let source_event_id = cron_notification_source_event_id(&pending.execution_id);

        // Replay guard, BEFORE any side effect. The append below is idempotent
        // on `source_event_id`, but the work leading up to it is not: a boot
        // re-drive of an already-delivered result (its transcript row landed
        // but the ledger stamp didn't) would otherwise re-read the fire's reply
        // and — worse, user-visibly — un-hide a conversation the user has since
        // removed. Detect the existing row first and just re-resolve the ledger.
        match self
            .volatile
            .session_manager
            .find_message_ordinal_by_source_event_id(&session_id, &source_event_id)
            .await
        {
            Ok(Some(_)) => {
                debug!(
                    session_id = %session_id,
                    execution_id = %pending.execution_id,
                    "cron result transcript row already exists; resolving replay without re-delivery"
                );
                self.resolve_cron_delivery(&pending.execution_id).await;
                return;
            }
            Ok(None) => {}
            Err(error) => {
                // Couldn't check — fall through to the append, which is itself
                // idempotent, rather than risk dropping a genuine first delivery.
                warn!(
                    session_id = %session_id,
                    execution_id = %pending.execution_id,
                    error = %error,
                    "could not check for an existing cron result row; proceeding to idempotent append"
                );
            }
        }

        let content = self.build_cron_notification(&pending).await;
        // A notification landing is exactly what makes a hidden conversation
        // relevant again — and the push deep-links here, so the conversation
        // must be in the user's list when they tap it.
        self.unhide_for_cron_notification().await;

        let outcome = match self
            .publish_cron_notification(content, Some(source_event_id))
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                // Leave the ledger unresolved: the boot re-drive retries the
                // same source-event key rather than losing the reminder.
                error!(
                    session_id = %session_id,
                    execution_id = %pending.execution_id,
                    error = %error,
                    "cron result delivery failed; leaving it for re-drive"
                );
                return;
            }
        };

        self.resolve_cron_delivery(&pending.execution_id).await;
        if matches!(outcome, SessionMessageAppendOutcome::Existing { .. }) {
            debug!(
                session_id = %session_id,
                execution_id = %pending.execution_id,
                "cron result transcript row already exists; resolved replay"
            );
            return;
        }
        info!(
            session_id = %session_id,
            execution_id = %pending.execution_id,
            cron_job_id = %pending.cron_job_id,
            "delivered cron result to origin conversation"
        );
    }

    /// The notification's content: a header naming the turn, the fire's own
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

    /// The fire's reply, read from its own session at the ordinal the turn's
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
                .handle_compact(slash_command_text(&content), sent_at, platform_msg_id)
                .await;
        }
        // Background results are not folded into slash turns. Their own
        // notification turn runs after this one, keeping the leading
        // `/command` intact for slash detection.
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
                TurnInput::UserChat { content },
                None,
                Some(response_tx),
                None,
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
    /// consecutive rows into one message for the provider call. The turn
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
        // content rides in `TurnInput` for the turn record only.
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
                TurnInput::UserChat { content: combined },
                None,
                Some(response_tx),
                Some(&mut interjections),
                Some(&mut stopped),
                None,
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
        self.settle_background_notification_after_user_turn(stopped, &response.content)
            .await;
        self.send_user_reply(response).await;
        Ok(())
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
    fn handle_set_model(
        &mut self,
        llm: Option<LlmEntryName>,
        model: Option<String>,
        effort: Option<String>,
    ) {
        debug!(
            session_id = %self.durable.session.id,
            llm = ?llm,
            model = ?model,
            effort = ?effort,
            "re-pinning session LLM",
        );
        // Keep the in-memory view consistent (the persisted columns are
        // authoritative on the next `get`); then swap the live loop.
        self.durable.session.state.last_llm = llm.clone();
        self.durable.session.state.last_model = model.clone();
        self.durable.session.state.last_effort = effort.clone();
        self.volatile.agent_loop.set_initial_llm(llm, model, effort);
    }

    /// Persist actor-owned session state and refresh activity so the reaper
    /// does not immediately target an actor that just completed durable work.
    async fn persist_session_state_with_activity(&mut self, context: &'static str) -> bool {
        self.durable.session.last_active = chrono::Utc::now();
        self.save_session_row(context).await
    }

    /// Final actor-state write without stamping `last_active = now`. The chat
    /// list is ordered by `last_active`, and the reaper stops actors
    /// *because* they are idle — stamping "now" there would hoist every
    /// reaped stale conversation above genuinely recent ones (and a gateway
    /// shutdown would flatten the whole list onto one timestamp). The
    /// keep-alive rationale for the bump doesn't apply to an exiting actor.
    ///
    /// The in-memory copy can't just be written back either: the router
    /// touches `last_active` straight in the store on every inbound message
    /// (`SessionManager::touch`), so the actor's copy is typically *older*
    /// than the row and a full save would regress recency. Merge the
    /// authoritative value first.
    async fn persist_session_state_preserving_activity(&mut self, context: &'static str) -> bool {
        match self
            .volatile
            .session_manager
            .get(&self.durable.session.id)
            .await
        {
            Ok(Some(stored)) if stored.last_active > self.durable.session.last_active => {
                self.durable.session.last_active = stored.last_active;
            }
            Ok(_) => {}
            Err(e) => {
                warn!(
                    session_id = %self.durable.session.id,
                    context = %context,
                    error = %e,
                    "could not read the stored session before the final save; \
                     using the in-memory activity timestamp"
                );
            }
        }
        self.save_session_row(context).await
    }

    async fn save_session_row(&mut self, context: &'static str) -> bool {
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
                    "failed to persist actor-owned session state; live actor still has the latest copy in memory"
                );
                false
            }
        }
    }

    /// Send a user-turn reply. A blank reply (no non-whitespace text) is
    /// anomalous for a user turn — the user is waiting — so surface a
    /// fallback `Notice` rather than push an empty bubble. (Non-user turns —
    /// cron, background notification — silently suppress a blank reply.)
    async fn send_user_reply(&self, response: OutgoingMessage) {
        if is_blank_reply(&response.content) {
            warn!(
                session_id = %self.durable.session.id,
                "user turn produced an empty reply; surfacing fallback notice"
            );
            // Record the fallback as an out-of-band control event so a reload
            // doesn't show a bare user turn with no reply; the live notice
            // carries the durable row id so clients dedup against the synced
            // twin.
            let durable_id = match self.current_after_ordinal().await {
                Some(after) => {
                    self.persist_control_event(
                        after,
                        ControlEventKind::NoticeWarn,
                        EMPTY_USER_REPLY_NOTICE,
                        chrono::Utc::now(),
                        "",
                    )
                    .await
                }
                None => None,
            };
            let notice = AgentOutput {
                session_id: self.durable.session.id.clone(),
                user_id: self.durable.session.user.id.clone(),
                channel: self.durable.session.channel.clone(),
                event: AgentEvent::Notice {
                    level: NoticeLevel::Warn,
                    text: EMPTY_USER_REPLY_NOTICE.to_string(),
                    mid_turn: false,
                    durable_id,
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
        platform_msg_id: String,
    ) -> anyhow::Result<()> {
        // `compact_now` mints its own maintenance turn and returns a control
        // notice; it does not emit a chat reply from the actor itself.
        let notice = self
            .volatile
            .agent_loop
            .compact_now(
                &mut self.durable.session,
                &self.volatile.turn_lifecycle,
                &self.volatile.span_recorder,
                None,
                self.volatile.actor_token.child_token(),
            )
            .await?;
        // A failed compaction is recorded — and re-rendered on reload — as the
        // warning it is, not as a confirmation.
        let event_kind = match notice.level {
            NoticeLevel::Warn => ControlEventKind::NoticeWarn,
            NoticeLevel::Error => ControlEventKind::NoticeError,
            NoticeLevel::Info => ControlEventKind::NoticeInfo,
        };
        // Record the `/compact` echo + its confirmation as out-of-band control
        // events (off the LLM transcript) so a reload shows them, anchored after
        // the (post-compaction) last row.
        let durable_id = match self.current_after_ordinal().await {
            Some(after) => {
                // The Command echo carries the send's id so a client's
                // optimistic `/compact` bubble reconciles with it; the notice
                // is server-authored, so it has none.
                self.persist_control_event(
                    after,
                    ControlEventKind::Command,
                    &command_text,
                    sent_at,
                    &platform_msg_id,
                )
                .await;
                self.persist_control_event(after, event_kind, &notice.text, chrono::Utc::now(), "")
                    .await
            }
            None => None,
        };
        let output = AgentOutput {
            session_id: self.durable.session.id.clone(),
            user_id: self.durable.session.user.id.clone(),
            channel: self.durable.session.channel.clone(),
            event: AgentEvent::Notice {
                level: notice.level,
                text: notice.text,
                mid_turn: false,
                durable_id,
            },
        };
        self.send_response(output, "compact").await;
        Ok(())
    }

    /// Run the agent loop for a subagent-spawned session. Distinct from
    /// `handle_user_input` because the TurnInput must be `Spawned` (not
    /// `UserChat`) so `TurnLifecycle::start_turn`'s allowed-for check
    /// passes regardless of the inherited trigger kind.
    async fn handle_subagent_spawned(
        &mut self,
        incoming: IncomingMessage,
        parent_turn_id: baybo_model::TurnId,
    ) -> anyhow::Result<()> {
        let content = incoming.message.content;
        let response_tx = self.volatile.response_tx.clone();
        self.volatile
            .agent_loop
            .append_spawned_prompt(content.clone())
            .await?;
        let response = self
            .run_agent_loop(
                TurnInput::Spawned {
                    initial_prompt: content,
                },
                Some(parent_turn_id),
                Some(response_tx),
                None,
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
    /// only on the chat view. Returns the event's stable transcript row id
    /// (`n<seq>`) so a live notice emitted for the same event can carry it
    /// (`AgentEvent::Notice::durable_id`) and clients dedup against the row the
    /// next sync redelivers. Best-effort: `None` on a write failure just means
    /// it won't reappear on reload (and the live notice dedups nothing).
    async fn persist_control_event(
        &self,
        after_ordinal: i64,
        kind: ControlEventKind,
        text: &str,
        at: chrono::DateTime<chrono::Utc>,
        platform_msg_id: &str,
    ) -> Option<String> {
        match self
            .volatile
            .session_manager
            .append_control_event(
                &self.durable.session.id,
                after_ordinal,
                kind,
                text,
                at,
                platform_msg_id,
            )
            .await
        {
            Ok(seq) => Some(baybo_model::control_event_row_id(seq)),
            Err(e) => {
                warn!(
                    session_id = %self.durable.session.id,
                    error = %e,
                    "failed to persist control event"
                );
                None
            }
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

    // The cancellation classification hinges on anyhow's downcast walking the
    // context chain: `run_agent_loop` tags a cancelled turn's error with
    // `.context(TurnCancelled)`, and the mailbox log sites downcast on it after
    // the error propagates up. Lock in that the marker survives — including
    // under further context a caller might add — and that a genuine failure is
    // never misclassified.
    #[test]
    fn turn_cancelled_marker_is_downcastable_through_context() {
        let tagged = anyhow::anyhow!("agent loop cancelled").context(TurnCancelled);
        assert!(is_turn_cancelled(&tagged));

        let wrapped = tagged.context("failed to handle user input");
        assert!(is_turn_cancelled(&wrapped));

        assert!(!is_turn_cancelled(&anyhow::anyhow!("provider 400")));
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
            builtin: None,
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
