use std::sync::Arc;

use aura_channels::{AgentOutput, IncomingMessage, JobOutcome, OutgoingMessage};
use aura_cron::TriggerAction;
use aura_hook::{HookContext, HookEventData, HookManager, HookPoint};
use aura_job::{JobInput, JobOutput};
use aura_model::{ApprovedResource, ContentBlock, JobId, MessageMetadata, Session};
use aura_tools::ToolOutput;
use aura_trace::{LifecycleOutcome, StepHandle, StepKind};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::agent_loop::AgentLoop;
use crate::job::JobLifecycle;
use crate::tool_executor::ToolExecutor;
use crate::trace::SpanRecorder;

/// Messages that can be sent to an AgentActor.
#[derive(Debug, Clone)]
pub enum AgentMessage {
    /// A user sent a message.
    UserInput(Box<IncomingMessage>),
    /// A cron job fired.
    CronTrigger {
        job_id: String,
        action: TriggerAction,
    },
    /// A subagent was spawned. Carries the initial prompt assembled by
    /// `LocalSubagentRuntime` and the parent's `JobId` for lineage.
    /// The child actor runs `agent_loop.run` with `JobInput::Spawned`,
    /// which `JobKind::Spawned.allowed_for(*) == true` lets through
    /// regardless of the child session's root trigger — which it must,
    /// because subagents inherit the parent's trigger (cron / system)
    /// via `create_spawned_session`.
    SubagentSpawned {
        initial_message: Box<IncomingMessage>,
        parent_job_id: aura_model::JobId,
    },
    /// Gracefully shut down this actor.
    Shutdown,
}

/// One actor per session. Receives messages sequentially through its mailbox.
pub struct AgentActor {
    session: Session,
    agent_loop: AgentLoop,
    tool_executor: Arc<ToolExecutor>,
    response_tx: mpsc::Sender<AgentOutput>,
    hooks: Arc<HookManager>,
    job_lifecycle: Arc<JobLifecycle>,
    span_recorder: Arc<SpanRecorder>,
    /// Lifetime token for this actor. Each per-job execution derives a
    /// child token from this; `Shutdown` on the mailbox trips it,
    /// cascading cancel down through every in-flight tool / subagent.
    actor_token: CancellationToken,
}

impl AgentActor {
    pub fn new(
        session: Session,
        agent_loop: AgentLoop,
        tool_executor: Arc<ToolExecutor>,
        response_tx: mpsc::Sender<AgentOutput>,
        hooks: Arc<HookManager>,
        job_lifecycle: Arc<JobLifecycle>,
        span_recorder: Arc<SpanRecorder>,
    ) -> Self {
        Self {
            session,
            agent_loop,
            tool_executor,
            response_tx,
            hooks,
            job_lifecycle,
            span_recorder,
            actor_token: CancellationToken::new(),
        }
    }

    /// Run the actor's message processing loop.
    pub async fn run(mut self, mut mailbox: mpsc::Receiver<AgentMessage>) {
        info!(session_id = %self.session.id, "agent actor started");

        while let Some(msg) = mailbox.recv().await {
            match msg {
                AgentMessage::UserInput(incoming) => {
                    if let Err(e) = self.handle_user_input(*incoming).await {
                        error!(
                            session_id = %self.session.id,
                            error = %e,
                            "failed to handle user input"
                        );
                    }
                }
                AgentMessage::CronTrigger { job_id, action } => {
                    debug!(session_id = %self.session.id, job_id = %job_id, "received cron trigger");
                    let result = match &action {
                        TriggerAction::Prompt { prompt } => {
                            let job_input = JobInput::Cron {
                                action_payload: serde_json::json!({
                                    "kind": "prompt",
                                    "cron_job_id": job_id,
                                    "prompt": prompt,
                                }),
                            };
                            self.dispatch_prompt(prompt, "cron", &job_id, job_input, None)
                                .await
                        }
                        TriggerAction::ToolCall {
                            tool_name,
                            params,
                            approved_resources,
                        } => {
                            self.handle_cron_tool_call(
                                &job_id,
                                tool_name,
                                params.clone(),
                                approved_resources.clone(),
                            )
                            .await
                        }
                    };
                    if let Err(e) = result {
                        error!(
                            session_id = %self.session.id,
                            job_id = %job_id,
                            error = %e,
                            "failed to handle cron trigger"
                        );
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
                        error!(
                            session_id = %self.session.id,
                            error = %e,
                            "failed to handle subagent spawn"
                        );
                    }
                }
                AgentMessage::Shutdown => {
                    debug!(session_id = %self.session.id, "actor shutting down");
                    // Trip the actor's lifetime token so any in-flight
                    // tool / subagent observes the cancel even if the
                    // mailbox-drain happens while a job is running.
                    self.actor_token.cancel();
                    break;
                }
            }
        }

        info!(session_id = %self.session.id, "agent actor stopped");
    }

    /// Dispatch a system-generated prompt (cron or routine) through the
    /// agent loop and send the response to the output channel.
    ///
    /// `job_input` records the trigger provenance (e.g. `JobInput::Cron`
    /// or `JobInput::System`) — this MUST match the session's root
    /// trigger or `JobLifecycle::start_job` will reject it. The
    /// synthesized `[{source}:{source_id}] {prompt}` content is what the
    /// LLM sees; the `JobInput` is purely for the Job record.
    async fn dispatch_prompt(
        &mut self,
        prompt: &str,
        source: &str,
        source_id: &str,
        job_input: JobInput,
        parent_job_id: Option<aura_model::JobId>,
    ) -> anyhow::Result<()> {
        let content = vec![ContentBlock::Text(format!(
            "[{source}:{source_id}] {prompt}"
        ))];
        let result = self
            .agent_loop
            .run(
                &mut self.session,
                job_input,
                content,
                &self.job_lifecycle,
                &self.span_recorder,
                parent_job_id,
                None,
                self.actor_token.child_token(),
            )
            .await;

        let outcome = match &result {
            Ok(_) => JobOutcome::Completed,
            Err(_) => JobOutcome::Failed,
        };
        let response = result?;

        if let Err(e) = self.response_tx.send(AgentOutput::Message(response)).await {
            warn!(error = %e, "failed to send {source} response to channel");
        }
        self.emit_job_completed(outcome).await;
        Ok(())
    }

    /// Emit a `JobCompleted` terminal signal on the response channel.
    /// `job_id` is `None` — current subscribers (subagent runtime) only
    /// need terminal-state detection, not job-level matching. Plumb the
    /// real id through `agent_loop.run`'s return when a subscriber starts to.
    async fn emit_job_completed(&self, outcome: JobOutcome) {
        let signal = AgentOutput::JobCompleted {
            session_id: self.session.id.clone(),
            job_id: None,
            outcome,
        };
        if let Err(e) = self.response_tx.send(signal).await {
            debug!(error = %e, "failed to send JobCompleted signal");
        }
    }

    /// Execute a tool directly for a cron job, falling back to LLM on failure.
    ///
    /// Wraps the call in its own `Job` + `StepKind::ToolDirect` step (one
    /// tool `Span`, no LLM span — the variant exists specifically so this
    /// "no-LLM iteration" doesn't masquerade as an `LlmIteration` in cost
    /// reports). On a crash this produces a half-open span the recovery
    /// scan will rewrite as `Cancelled { SystemCrash }` and fold into the
    /// job's `partial_artifacts`.
    async fn handle_cron_tool_call(
        &mut self,
        cron_job_id: &str,
        tool_name: &str,
        params: serde_json::Value,
        pre_approved: Vec<ApprovedResource>,
    ) -> anyhow::Result<()> {
        // 1. Open a Job for this cron action.
        let job = self
            .job_lifecycle
            .start_job(
                self.session.id.clone(),
                self.session.trigger.kind(),
                JobInput::Cron {
                    action_payload: serde_json::json!({
                        "kind": "tool_call",
                        "cron_job_id": cron_job_id,
                        "tool_name": tool_name,
                        "params": params,
                    }),
                },
                self.session.bound_soul_version.clone(),
                None,
            )
            .await?;
        self.job_lifecycle.start(&job.id).await?;

        // 2. Open a Step for the iteration.
        let step = self
            .span_recorder
            .begin_step(job.id, StepKind::ToolDirect)
            .await?;

        let approved = std::sync::Arc::new(parking_lot::Mutex::new(pre_approved));

        let tool_result = self
            .tool_executor
            .execute(
                tool_name,
                params.clone(),
                &self.session.id,
                &self.session.user,
                &approved,
                &self.span_recorder,
                &step,
                None,          // no triggering LLM span — direct cron invocation
                String::new(), // no tool_use_id — cron tools don't pair back to an LLM tool_use block
                None,          // no parallel group
                Some(job.id),
                self.actor_token.child_token(),
            )
            .await;

        let output = match tool_result {
            Ok(ToolOutput::Error(e)) => {
                return self
                    .fail_cron_tool_and_diagnose(step, job.id, cron_job_id, tool_name, e)
                    .await;
            }
            Ok(output) => output,
            Err(e) => {
                return self
                    .fail_cron_tool_and_diagnose(step, job.id, cron_job_id, tool_name, e)
                    .await;
            }
        };

        self.span_recorder
            .end_step(step, LifecycleOutcome::Ok)
            .await
            .ok();

        let (text, attachments) = match &output {
            ToolOutput::Text(t) => (t.clone(), Vec::new()),
            ToolOutput::Json(v) => (
                serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()),
                Vec::new(),
            ),
            ToolOutput::WithAttachments { text, attachments } => {
                (text.clone(), attachments.clone())
            }
            ToolOutput::MultiModalText { text, llm_images } => (text.clone(), llm_images.clone()),
            ToolOutput::Error(_) => unreachable!("handled above"),
        };

        // Carry attachments through so a cron-scheduled SendFile
        // actually delivers the file.
        let mut content = vec![ContentBlock::Text(format!("[cron:{cron_job_id}] {text}"))];
        content.extend(attachments);
        let response = OutgoingMessage {
            session_id: self.session.id.to_string(),
            user_id: self.session.user.id.clone(),
            channel: self.session.user.channel.clone(),
            content: content.clone(),
            reply_to: None,
            metadata: MessageMetadata::default(),
        };
        self.job_lifecycle
            .complete(
                &job.id,
                JobOutput::Message {
                    content: response.content.clone(),
                },
            )
            .await
            .ok();
        if let Err(e) = self.response_tx.send(AgentOutput::Message(response)).await {
            warn!(error = %e, "failed to send cron tool result to channel");
        }
        self.emit_job_completed(JobOutcome::Completed).await;
        Ok(())
    }

    /// Common Failed-then-LLM-diagnose path shared by both
    /// `Ok(ToolOutput::Error(...))` and `Err(...)` outcomes of a cron
    /// direct-tool call. Hands off to a child diagnostic job that
    /// references the failed tool job via `parent_job_id` so lineage
    /// queries can show the chain.
    async fn fail_cron_tool_and_diagnose(
        &mut self,
        step: StepHandle,
        job_id: JobId,
        cron_job_id: &str,
        tool_name: &str,
        error: impl std::fmt::Display,
    ) -> anyhow::Result<()> {
        let reason = error.to_string();
        self.span_recorder
            .end_step(
                step,
                LifecycleOutcome::Failed {
                    reason: reason.clone(),
                },
            )
            .await
            .ok();
        self.job_lifecycle.fail(&job_id, reason.clone()).await.ok();
        self.emit_job_completed(JobOutcome::Failed).await;
        warn!(
            cron_job_id = %cron_job_id,
            tool = %tool_name,
            error = %reason,
            "cron tool call failed, falling back to LLM"
        );
        let diagnostic = format!(
            "[cron:{cron_job_id}] Tool '{tool_name}' failed: {reason}\n\
             Diagnose the issue and report to the user.",
        );
        let diag_input = JobInput::Cron {
            action_payload: serde_json::json!({
                "kind": "tool_failure_diagnostic",
                "cron_job_id": cron_job_id,
                "tool_name": tool_name,
                "failed_tool_job_id": job_id.to_string(),
                "error": reason,
            }),
        };
        self.dispatch_prompt(&diagnostic, "cron", cron_job_id, diag_input, Some(job_id))
            .await
    }

    async fn handle_user_input(&mut self, incoming: IncomingMessage) -> anyhow::Result<()> {
        let message_clone = incoming.message.clone();
        let content = incoming.message.content;

        // PreMessage hook
        let mut hook_ctx = HookContext {
            session_id: self.session.id.to_string(),
            user_id: Some(self.session.user.id.clone()),
            event_data: HookEventData::PreMessage,
            message: Some(message_clone),
            response: None,
            job_id: None,
            trace_span_id: None,
            extra: Default::default(),
        };
        self.hooks
            .trigger(HookPoint::PreMessage, &mut hook_ctx)
            .await?;

        // Run the agent loop. Pass a clone of the response channel so the
        // loop can stream text deltas as `AgentOutput::Delta` while the
        // final assembled message still flows through the normal path.
        let result = self
            .agent_loop
            .run(
                &mut self.session,
                JobInput::UserChat {
                    content: content.clone(),
                },
                content,
                &self.job_lifecycle,
                &self.span_recorder,
                None,
                Some(self.response_tx.clone()),
                self.actor_token.child_token(),
            )
            .await;
        let outcome = match &result {
            Ok(_) => JobOutcome::Completed,
            Err(_) => JobOutcome::Failed,
        };
        let response = result?;

        // PreResponse hook
        let mut hook_ctx = HookContext {
            session_id: self.session.id.to_string(),
            user_id: Some(self.session.user.id.clone()),
            event_data: HookEventData::PreResponse,
            message: None,
            response: Some(response.clone()),
            job_id: None,
            trace_span_id: None,
            extra: Default::default(),
        };
        self.hooks
            .trigger(HookPoint::PreResponse, &mut hook_ctx)
            .await?;

        // Send response
        if let Err(e) = self.response_tx.send(AgentOutput::Message(response)).await {
            warn!(error = %e, "failed to send response to channel");
        }
        self.emit_job_completed(outcome).await;

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
        let result = self
            .agent_loop
            .run(
                &mut self.session,
                JobInput::Spawned {
                    initial_prompt: content.clone(),
                },
                content,
                &self.job_lifecycle,
                &self.span_recorder,
                Some(parent_job_id),
                Some(self.response_tx.clone()),
                self.actor_token.child_token(),
            )
            .await;
        let outcome = match &result {
            Ok(_) => JobOutcome::Completed,
            Err(_) => JobOutcome::Failed,
        };
        let response = result?;

        if let Err(e) = self.response_tx.send(AgentOutput::Message(response)).await {
            warn!(error = %e, "failed to send subagent response");
        }
        self.emit_job_completed(outcome).await;

        Ok(())
    }
}
