//! Actor model + supervision: one `AgentActor` per session, supervised
//! by [`AgentSupervisor`](crate::actor::supervisor::AgentSupervisor),
//! routed to by [`Router`](crate::actor::router::Router), and
//! checkpointed via [`DurableActorState`](crate::actor::state::DurableActorState).

pub mod cron_prompt;
pub mod mailbox;
pub mod router;
pub mod state;
pub mod subagent;
pub mod supervisor;

use aura_channels::{AgentOutput, COMPACT_COMMAND, IncomingMessage, NoticeLevel, OutgoingMessage};
use aura_job::JobInput;
use aura_model::{BackgroundCompressionPayload, ContentBlock, PendingSubagentResult};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Hard cap on `session.state.pending_subagent_results`. A parent
/// that stays idle while many background subagents finish would
/// otherwise grow this vec without bound — both in memory and on
/// the persisted row. Once the cap is reached the oldest entry is
/// dropped (its content still lives in the child session's trace).
const MAX_PENDING_SUBAGENT_RESULTS: usize = 64;

/// Opening framing for a `SubagentNotification` turn's content. Lives in
/// per-turn content (never the system prompt) so the prompt-cache prefix
/// is identical to a normal main-path turn. Cron-style: report proactively.
const SUBAGENT_NOTIFICATION_FRAMING: &str = "[background subagent task(s) finished since your last turn — report the outcome to the user as a fresh, proactive message.]";

/// Per-result element of the nested `<subagent_results>` block. Metadata
/// rides as attributes; `task` / `output` are child elements so multi-line
/// free text with quotes needs no attribute escaping.
const SUBAGENT_RESULT_TEMPLATE: &str = r#"  <result handle="{{handle}}" type="{{type}}" status="{{status}}">
    <task>{{task}}</task>
    <output>{{output}}</output>
  </result>
"#;

use crate::actor::state::{DurableActorState, VolatileResources};
use crate::actor::supervisor::ActorRegistryGuard;

/// Messages that can be sent to an AgentActor.
#[derive(Debug, Clone)]
pub enum AgentMessage {
    /// A user sent a message.
    UserInput(Box<IncomingMessage>),
    /// A cron job fired.
    CronTrigger { job_id: String, prompt: String },
    /// A subagent was spawned. Carries the initial prompt assembled by
    /// `Router::handle_subagent_spawn` and the parent's `JobId` for
    /// lineage. The child actor runs `agent_loop.run` with `JobInput::Spawned`,
    /// which `JobKind::Spawned.allowed_for(*) == true` lets through
    /// regardless of the child session's root trigger — which it must,
    /// because subagents inherit the parent's trigger (cron / system)
    /// via `create_spawned_session`.
    SubagentSpawned {
        initial_message: Box<IncomingMessage>,
        parent_job_id: aura_model::JobId,
    },
    /// A maintenance task has been spawned on this actor's session.
    /// Bypasses the normal chat-turn cycle (`agent_loop.run`) and
    /// dispatches the carried payload to a dedicated handler.
    BackgroundCompression(BackgroundCompressionPayload),
    /// A `background: true` subagent dispatched from this session
    /// reached a terminal state. The wait task posts this to the parent
    /// actor's mailbox; it is buffered on
    /// `session.state.pending_subagent_results` and, once no
    /// higher-priority work is queued, drained into one autonomous
    /// `SubagentNotification` turn.
    SubagentFinished(Box<PendingSubagentResult>),
    /// Stop this actor. Lowest mailbox priority — every queued
    /// `UserInput` / `SubagentFinished` drains first, then the actor
    /// trips its `actor_token` and exits. (The session row lives on; only
    /// the in-memory actor stops.)
    ActorStop,
}

impl mailbox::Prioritized for AgentMessage {
    fn priority(&self) -> mailbox::MessagePriority {
        use mailbox::MessagePriority;
        match self {
            AgentMessage::UserInput(_)
            | AgentMessage::CronTrigger { .. }
            | AgentMessage::SubagentSpawned { .. }
            | AgentMessage::BackgroundCompression(_) => MessagePriority::Trigger,
            AgentMessage::SubagentFinished(_) => MessagePriority::SubagentFinished,
            AgentMessage::ActorStop => MessagePriority::Stop,
        }
    }
}

/// Render a short status label for the background-notification
/// preamble — keeps the LLM's reading short and stable.
fn pending_status_label(status: &aura_model::SubagentExitStatus) -> &'static str {
    match status {
        aura_model::SubagentExitStatus::Completed => "completed",
        aura_model::SubagentExitStatus::Cancelled => "cancelled",
        aura_model::SubagentExitStatus::Failed { .. } => "failed",
        aura_model::SubagentExitStatus::Timeout => "timeout",
    }
}

/// Cap a subagent's final text when rendered into the parent's
/// next-turn reminder. The full content is still in the trace and
/// the persisted child session; this cap exists only so a giant
/// final message can't dominate the parent's context budget.
fn truncate_for_notice(text: &str) -> String {
    const MAX: usize = 1024;
    if text.chars().count() <= MAX {
        return text.to_string();
    }
    let truncated: String = text.chars().take(MAX).collect();
    format!("{truncated}… [truncated; full text in child session transcript]")
}

/// Escape the XML metacharacters relevant to attribute values and element
/// bodies so a subagent's free text can't break the `<subagent_results>`
/// structure the parent LLM reads.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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

        while let Some(msg) = mailbox.recv().await {
            let deferred = match msg {
                AgentMessage::ActorStop => {
                    debug!(session_id = %session_id, "actor stopping");
                    // Cancelling our `actor_token` cascades into every
                    // child we spawned — including maintenance actors,
                    // whose `actor_token` is a grandchild of ours via
                    // the `parent_actor_token` carried in their
                    // `SystemSpawnRequest`. No explicit dispatch needed.
                    self.volatile.actor_token.cancel();
                    break;
                }
                // Coalesce a leading run of NON-slash `UserInput`s already
                // queued behind this one into a single turn (rapid bursts
                // the actor was too busy to take one at a time). A slash
                // message is a hard boundary — it falls through to
                // `dispatch_one` so its leading `/` stays at content
                // position 0 for compact / skill detection.
                AgentMessage::UserInput(incoming)
                    if !is_slash_command(&incoming.message.content) =>
                {
                    let mut batch = vec![*incoming];
                    let mut deferred = None;
                    while matches!(
                        mailbox.peek_priority(),
                        Some(mailbox::MessagePriority::Trigger)
                    ) {
                        match mailbox.try_recv() {
                            Ok(AgentMessage::UserInput(inc))
                                if !is_slash_command(&inc.message.content) =>
                            {
                                batch.push(*inc);
                            }
                            // A queued slash `UserInput` (or, on non-user
                            // sessions, another trigger kind) ends the run;
                            // handle it on its own after the batch.
                            Ok(other) => {
                                deferred = Some(other);
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    if let Err(e) = self.handle_merged_user_turn(batch).await {
                        error!(
                            session_id = %session_id,
                            error = %e,
                            "failed to handle user input"
                        );
                    }
                    deferred
                }
                other => {
                    self.dispatch_one(other).await;
                    None
                }
            };
            if let Some(msg) = deferred {
                self.dispatch_one(msg).await;
            }
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
            AgentMessage::BackgroundCompression(payload) => {
                if let Err(e) = self.handle_background_compression(payload).await {
                    error!(session_id = %session_id, error = %e, "failed to handle background compression");
                }
            }
            AgentMessage::SubagentFinished(pending) => {
                self.handle_subagent_finished(*pending).await;
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
    /// (`subscribe_terminal_events`); the actor no longer emits a
    /// piggy-back signal on the response channel. Used by every
    /// handler that delegates job lifecycle to `agent_loop.run`
    /// (UserInput, SubagentSpawned, cron prompt dispatch). Returns
    /// the loop's response on `Ok`; caller is responsible for sending
    /// it to the response channel.
    async fn run_agent_loop(
        &mut self,
        job_input: JobInput,
        content: Vec<ContentBlock>,
        parent_job_id: Option<aura_model::JobId>,
        delta_tx: Option<mpsc::Sender<AgentOutput>>,
    ) -> anyhow::Result<OutgoingMessage> {
        self.volatile
            .agent_loop
            .run(
                &mut self.durable.session,
                job_input,
                content,
                &self.volatile.job_lifecycle,
                &self.volatile.span_recorder,
                parent_job_id,
                delta_tx,
                self.volatile.actor_token.child_token(),
            )
            .await
    }

    /// Dispatch a fired cron job through the agent loop and send the
    /// response to the output channel.
    ///
    /// The `JobInput::Cron` provenance must match the session's root
    /// trigger or `JobLifecycle::start_job` will reject it; the cron fire
    /// mints a Cron-rooted session, so it does. The content the LLM sees
    /// is framed by [`cron_prompt::frame_cron_prompt`] so the model treats
    /// the fire as a task to perform now rather than a live user message.
    async fn dispatch_cron_prompt(&mut self, prompt: &str, job_id: &str) -> anyhow::Result<()> {
        let job_input = JobInput::Cron {
            action_payload: serde_json::json!({
                "cron_job_id": job_id,
                "prompt": prompt,
            }),
        };
        let content = vec![ContentBlock::Text(cron_prompt::frame_cron_prompt(
            job_id, prompt,
        ))];
        let response = self
            .run_agent_loop(job_input, content, None, None)
            .await?;
        self.send_response(AgentOutput::Message(response), "cron")
            .await;
        Ok(())
    }

    async fn handle_user_input(&mut self, incoming: IncomingMessage) -> anyhow::Result<()> {
        let content = incoming.message.content;
        if is_compact_command(&content) {
            return self.handle_compact().await;
        }
        // Background `spawn_subagent` results are NOT folded into the
        // user's turn — they run as their own `SubagentNotification` turn
        // (scheduled by `maybe_run_subagent_notification`) so the user's
        // turn keeps a clean leading `/command` for slash detection.
        //
        // Pass a clone of the response channel so the loop can stream
        // text deltas as `AgentOutput::Delta` while the final assembled
        // message still flows through the normal path.
        let response_tx = self.volatile.response_tx.clone();
        let response = self
            .run_agent_loop(
                JobInput::UserChat {
                    content: content.clone(),
                },
                content,
                None,
                Some(response_tx),
            )
            .await?;
        self.send_response(AgentOutput::Message(response), "user")
            .await;
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
        // Append the leading messages as their own rows ahead of the turn.
        for incoming in batch {
            self.volatile
                .agent_loop
                .append_user_message(&self.durable.session, incoming.message.content)
                .await?;
        }
        let response_tx = self.volatile.response_tx.clone();
        let response = self
            .run_agent_loop(
                JobInput::UserChat { content: combined },
                last.message.content,
                None,
                Some(response_tx),
            )
            .await?;
        self.send_response(AgentOutput::Message(response), "user")
            .await;
        Ok(())
    }

    /// Idempotent on `handle_id` — the wait task can in principle
    /// publish twice (mailbox retry, manual recovery) and we don't
    /// want the notification to list it twice. Capped at
    /// `MAX_PENDING_SUBAGENT_RESULTS` with drop-oldest semantics so a
    /// parent that stays idle while many backgrounds finish can't
    /// grow the persisted row without bound.
    async fn handle_subagent_finished(&mut self, pending: PendingSubagentResult) {
        let buffer = &mut self.durable.session.state.pending_subagent_results;
        if buffer.iter().any(|p| p.handle_id == pending.handle_id) {
            debug!(
                session_id = %self.durable.session.id,
                handle_id = %pending.handle_id,
                "duplicate SubagentFinished for handle; ignoring"
            );
            return;
        }
        debug!(
            session_id = %self.durable.session.id,
            handle_id = %pending.handle_id,
            subagent_type = %pending.subagent_type,
            "buffered background subagent result for its notification turn"
        );
        if buffer.len() >= MAX_PENDING_SUBAGENT_RESULTS {
            let dropped = buffer.remove(0);
            warn!(
                session_id = %self.durable.session.id,
                dropped_handle_id = %dropped.handle_id,
                cap = MAX_PENDING_SUBAGENT_RESULTS,
                "pending subagent buffer full; dropping oldest entry"
            );
        }
        buffer.push(pending);
        self.persist_session_state_after_pending_change("subagent_finished")
            .await;
    }

    /// Write the actor's current `durable.session` back to the
    /// session store so the latest `pending_subagent_results` survives
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

    /// Render the pending background-subagent results into nested-XML
    /// content for one `SubagentNotification` turn. Pure (does not drain) —
    /// the caller owns the buffer so it can restore the results if the turn
    /// fails. The framing rides in this per-turn content (never the system
    /// prompt) so the prompt-cache prefix stays identical to a normal
    /// main-path turn.
    fn build_subagent_notification_content(
        &self,
        pending: &[PendingSubagentResult],
    ) -> Vec<ContentBlock> {
        let mut xml = String::from(SUBAGENT_NOTIFICATION_FRAMING);
        xml.push_str("\n\n<subagent_results>\n");
        for p in pending {
            xml.push_str(
                &SUBAGENT_RESULT_TEMPLATE
                    .replace("{{handle}}", &xml_escape(&p.handle_id))
                    .replace("{{type}}", &xml_escape(&p.subagent_type))
                    .replace("{{status}}", pending_status_label(&p.status))
                    .replace("{{task}}", &xml_escape(&p.task_summary))
                    .replace("{{output}}", &xml_escape(&truncate_for_notice(&p.final_text))),
            );
        }
        xml.push_str("</subagent_results>");
        vec![ContentBlock::Text(xml)]
    }

    /// Surface buffered background-subagent results as their own turn —
    /// but only when no higher-priority work is queued: a `Trigger`
    /// (UserInput) must run first, and another queued `SubagentFinished`
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
            .pending_subagent_results
            .is_empty()
        {
            return;
        }
        if matches!(
            mailbox.peek_priority(),
            Some(p) if p >= mailbox::MessagePriority::SubagentFinished
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
        // Take the results but keep them in hand: the turn below is
        // fallible (provider error, cost rejection, cancellation), and a
        // delivered completion must not be lost if it fails. The actor is
        // single-threaded, so nothing else mutates the buffer while the
        // turn runs.
        let pending = std::mem::take(&mut self.durable.session.state.pending_subagent_results);
        if pending.is_empty() {
            return;
        }
        let content = self.build_subagent_notification_content(&pending);
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
        let result = self
            .run_agent_loop(
                JobInput::SubagentNotification {
                    content: content.clone(),
                },
                content,
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
                self.send_response(AgentOutput::Message(response), "subagent_notification")
                    .await;
            }
            Err(e) => {
                // Restore the drained results so the next drain retries
                // them — the child trace alone would never resurface.
                error!(
                    session_id = %self.durable.session.id,
                    error = %e,
                    "subagent-notification turn failed; restoring pending results for retry"
                );
                let mut restored = pending;
                restored.append(&mut self.durable.session.state.pending_subagent_results);
                self.durable.session.state.pending_subagent_results = restored;
                self.persist_session_state_after_pending_change("subagent_notification_restore")
                    .await;
            }
        }
    }

    /// `/compact` is a control command, not an assistant turn, so the
    /// confirmation goes back as `Notice` (out-of-band, off-transcript)
    /// rather than `Message`.
    async fn handle_compact(&mut self) -> anyhow::Result<()> {
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
        let notice = AgentOutput::Notice {
            session_id: self.durable.session.id.clone(),
            user_id: self.durable.session.user.id.clone(),
            channel: self.durable.session.channel.clone(),
            level: NoticeLevel::Info,
            text,
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
        parent_job_id: aura_model::JobId,
    ) -> anyhow::Result<()> {
        let content = incoming.message.content;
        let response_tx = self.volatile.response_tx.clone();
        let response = self
            .run_agent_loop(
                JobInput::Spawned {
                    initial_prompt: content.clone(),
                },
                content,
                Some(parent_job_id),
                Some(response_tx),
            )
            .await?;
        self.send_response(AgentOutput::Message(response), "subagent")
            .await;
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

    /// Run a background compression pass on this actor's session via
    /// `agent_loop.run_background_compression`.
    async fn handle_background_compression(
        &mut self,
        payload: BackgroundCompressionPayload,
    ) -> anyhow::Result<()> {
        let outcome = self
            .volatile
            .agent_loop
            .run_background_compression(
                &mut self.durable.session,
                payload,
                &self.volatile.job_lifecycle,
                &self.volatile.span_recorder,
                self.volatile.actor_token.child_token(),
            )
            .await?;
        debug!(
            session_id = %self.durable.session.id,
            cursor = outcome.cursor,
            cost_micros = outcome.cost_micros,
            "background summary: pass landed"
        );
        Ok(())
    }
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
        }]));
    }

    #[test]
    fn pending_status_label_covers_every_exit_status() {
        use aura_model::SubagentExitStatus as S;
        assert_eq!(pending_status_label(&S::Completed), "completed");
        assert_eq!(pending_status_label(&S::Cancelled), "cancelled");
        assert_eq!(
            pending_status_label(&S::Failed {
                reason: "x".into()
            }),
            "failed"
        );
        assert_eq!(pending_status_label(&S::Timeout), "timeout");
    }

    #[test]
    fn truncate_for_notice_appends_marker_when_over_cap() {
        let long = "a".repeat(2000);
        let out = truncate_for_notice(&long);
        assert!(
            out.len() > 1024,
            "marker must be appended on overflow: {out:?}"
        );
        assert!(out.contains("truncated"));
        let short = "hello";
        assert_eq!(truncate_for_notice(short), "hello");
    }
}
