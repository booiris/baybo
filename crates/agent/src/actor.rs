use std::sync::Arc;

use aura_channels::{AgentOutput, IncomingMessage, OutgoingMessage};
use aura_cron::TriggerAction;
use aura_job::{JobInput, JobOutput};
use aura_model::{ApprovedResource, ContentBlock, MessageMetadata, Session};
use aura_tools::ToolOutput;
use aura_trace::StepKind;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::agent_loop::AgentLoop;
use crate::job::{JobLifecycle, JobSpec};
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
    job_lifecycle: Arc<JobLifecycle>,
    span_recorder: Arc<SpanRecorder>,
    /// Lifetime token for this actor. Derived as a child of the
    /// process-wide parent token at construction time, so cancelling
    /// the parent (e.g. on `ShutdownSignal::trigger`) cascades down
    /// through every in-flight tool / subagent. `Shutdown` on the
    /// mailbox additionally trips it locally for cooperative shutdown
    /// of just this actor.
    actor_token: CancellationToken,
}

impl AgentActor {
    pub fn new(
        session: Session,
        agent_loop: AgentLoop,
        tool_executor: Arc<ToolExecutor>,
        response_tx: mpsc::Sender<AgentOutput>,
        job_lifecycle: Arc<JobLifecycle>,
        span_recorder: Arc<SpanRecorder>,
        parent_token: &CancellationToken,
    ) -> Self {
        Self {
            session,
            agent_loop,
            tool_executor,
            response_tx,
            job_lifecycle,
            span_recorder,
            actor_token: parent_token.child_token(),
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
        self.agent_loop
            .run(
                &mut self.session,
                job_input,
                content,
                &self.job_lifecycle,
                &self.span_recorder,
                parent_job_id,
                delta_tx,
                self.actor_token.child_token(),
            )
            .await
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
        let response = self
            .run_agent_loop(job_input, content, parent_job_id, None)
            .await?;
        if let Err(e) = self.response_tx.send(AgentOutput::Message(response)).await {
            warn!(error = %e, "failed to send {source} response to channel");
        }
        Ok(())
    }

    /// Execute a tool directly for a cron job.
    ///
    /// Wraps the call in its own `Job` + `StepKind::ToolDirect` step (one
    /// tool `Span`, no LLM span — the variant exists specifically so this
    /// "no-LLM iteration" doesn't masquerade as an `LlmIteration` in cost
    /// reports). On a crash this produces a half-open span the recovery
    /// scan will rewrite as `Cancelled { SystemCrash }` and fold into the
    /// job's `partial_artifacts`. Tool-execution failures propagate as
    /// `Err` to the actor's run loop, which logs them; there is no
    /// LLM-narrated diagnostic follow-up.
    async fn handle_cron_tool_call(
        &mut self,
        cron_job_id: &str,
        tool_name: &str,
        params: serde_json::Value,
        pre_approved: Vec<ApprovedResource>,
    ) -> anyhow::Result<()> {
        let spec = JobSpec {
            session_id: self.session.id.clone(),
            session_trigger_kind: self.session.trigger.kind(),
            input: JobInput::Cron {
                action_payload: serde_json::json!({
                    "kind": "tool_call",
                    "cron_job_id": cron_job_id,
                    "tool_name": tool_name,
                    "params": params,
                }),
            },
            effective_soul_version: self.session.bound_soul_version.clone(),
            parent_job_id: None,
        };
        let job_token = self.actor_token.child_token();

        let span_recorder = Arc::clone(&self.span_recorder);
        let tool_executor = Arc::clone(&self.tool_executor);
        let session_id = self.session.id.clone();
        let session_user = self.session.user.clone();
        let cron_id_for_body = cron_job_id.to_string();
        let tool_name_for_body = tool_name.to_string();
        let approved = std::sync::Arc::new(parking_lot::Mutex::new(pre_approved));
        let body_token = job_token.clone();

        let cancel_for_step = body_token.clone();
        let recorder_for_step = Arc::clone(&span_recorder);
        let response =
            crate::scope::with_job(&self.job_lifecycle, job_token, spec, |job_id| async move {
                let (job_output, response) = crate::scope::with_step(
                    recorder_for_step.as_ref(),
                    job_id,
                    StepKind::ToolDirect,
                    Some((&cancel_for_step, aura_job::CancelReason::ParentCancelled)),
                    |step| async move {
                        let output = match tool_executor
                            .execute(
                                &tool_name_for_body,
                                params,
                                &session_id,
                                &session_user,
                                &approved,
                                &span_recorder,
                                &step,
                                None,          // no triggering LLM span — direct cron invocation
                                String::new(), // no tool_use_id — cron tools don't pair back to an LLM tool_use block
                                None,          // no parallel group
                                Some(job_id),
                                body_token,
                            )
                            .await?
                        {
                            ToolOutput::Error(e) => return Err(anyhow::anyhow!(e.to_string())),
                            output => output,
                        };

                        let (text, attachments) = match &output {
                            ToolOutput::Text(t) => (t.clone(), Vec::new()),
                            ToolOutput::Json(v) => (
                                serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()),
                                Vec::new(),
                            ),
                            ToolOutput::WithAttachments { text, attachments } => {
                                (text.clone(), attachments.clone())
                            }
                            ToolOutput::MultiModalText { text, llm_images } => {
                                (text.clone(), llm_images.clone())
                            }
                            ToolOutput::Error(_) => unreachable!("handled above"),
                        };

                        // Carry attachments through so a cron-scheduled
                        // SendFile actually delivers the file.
                        let mut content = vec![ContentBlock::Text(format!(
                            "[cron:{cron_id_for_body}] {text}"
                        ))];
                        content.extend(attachments);
                        let response = OutgoingMessage {
                            session_id: session_id.to_string(),
                            user_id: session_user.id.clone(),
                            channel: session_user.channel.clone(),
                            content: content.clone(),
                            reply_to: None,
                            metadata: MessageMetadata::default(),
                        };
                        let job_output = JobOutput::Message {
                            content: response.content.clone(),
                        };
                        Ok((aura_trace::LifecycleOutcome::Ok, (job_output, response)))
                    },
                )
                .await?;
                Ok((job_output, response))
            })
            .await?;

        if let Err(e) = self.response_tx.send(AgentOutput::Message(response)).await {
            warn!(error = %e, "failed to send cron tool result to channel");
        }
        Ok(())
    }

    async fn handle_user_input(&mut self, incoming: IncomingMessage) -> anyhow::Result<()> {
        let content = incoming.message.content;
        // Sidecars carry `bot_id` on every `Frame::Message`; the session
        // itself was created without one (the resolver keys by channel +
        // user, not bot). Treating the latest message as authoritative
        // for bot context lets multi-bot MCP routing land the right
        // `_meta.auraBotId` at tool dispatch time without reshaping
        // `ChannelSessionStore`.
        self.session.user.bot_id = incoming.message.sender.bot_id.clone();

        // Pass a clone of the response channel so the loop can stream
        // text deltas as `AgentOutput::Delta` while the final assembled
        // message still flows through the normal path.
        let response = self
            .run_agent_loop(
                JobInput::UserChat {
                    content: content.clone(),
                },
                content,
                None,
                Some(self.response_tx.clone()),
            )
            .await?;
        if let Err(e) = self.response_tx.send(AgentOutput::Message(response)).await {
            warn!(error = %e, "failed to send response to channel");
        }
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
        let response = self
            .run_agent_loop(
                JobInput::Spawned {
                    initial_prompt: content.clone(),
                },
                content,
                Some(parent_job_id),
                Some(self.response_tx.clone()),
            )
            .await?;
        if let Err(e) = self.response_tx.send(AgentOutput::Message(response)).await {
            warn!(error = %e, "failed to send subagent response");
        }
        Ok(())
    }
}
