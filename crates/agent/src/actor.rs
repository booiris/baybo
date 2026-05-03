use std::sync::Arc;

use aura_channels::{AgentOutput, IncomingMessage, OutgoingMessage};
use aura_cron::TriggerAction;
use aura_hook::{HookContext, HookEventData, HookManager, HookPoint};
use aura_model::Session;
use aura_model::{ApprovedResource, ContentBlock, MessageMetadata};
use aura_tools::ToolOutput;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::agent_loop::AgentLoop;
use crate::observability::ObservabilityRecorder;
use crate::tool_executor::ToolExecutor;

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
    recorder: Arc<ObservabilityRecorder>,
}

impl AgentActor {
    pub fn new(
        session: Session,
        agent_loop: AgentLoop,
        tool_executor: Arc<ToolExecutor>,
        response_tx: mpsc::Sender<AgentOutput>,
        hooks: Arc<HookManager>,
        recorder: Arc<ObservabilityRecorder>,
    ) -> Self {
        Self {
            session,
            agent_loop,
            tool_executor,
            response_tx,
            hooks,
            recorder,
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
                    self.flush_trace().await;
                }
                AgentMessage::CronTrigger { job_id, action } => {
                    debug!(session_id = %self.session.id, job_id = %job_id, "received cron trigger");
                    let result = match &action {
                        TriggerAction::Prompt { prompt } => {
                            self.dispatch_prompt(prompt, "cron", &job_id).await
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
                    self.flush_trace().await;
                }
                AgentMessage::Shutdown => {
                    debug!(session_id = %self.session.id, "actor shutting down");
                    break;
                }
            }
        }

        // Flush observability data
        if let Err(e) = self.recorder.flush().await {
            warn!(error = %e, "failed to flush observability data on shutdown");
        }

        info!(session_id = %self.session.id, "agent actor stopped");
    }

    /// Dispatch a system-generated prompt (cron or routine) through the agent loop
    /// and send the response to the output channel.
    async fn dispatch_prompt(
        &mut self,
        prompt: &str,
        source: &str,
        source_id: &str,
    ) -> anyhow::Result<()> {
        let content = vec![ContentBlock::Text(format!(
            "[{source}:{source_id}] {prompt}"
        ))];
        let response = self
            .agent_loop
            .run(&mut self.session, content, &self.recorder, None, None)
            .await?;

        if let Err(e) = self.response_tx.send(AgentOutput::Message(response)).await {
            warn!(error = %e, "failed to send {source} response to channel");
        }
        Ok(())
    }

    /// Execute a tool directly for a cron job, falling back to LLM on failure.
    async fn handle_cron_tool_call(
        &mut self,
        job_id: &str,
        tool_name: &str,
        params: serde_json::Value,
        pre_approved: Vec<ApprovedResource>,
    ) -> anyhow::Result<()> {
        let approved = std::sync::Arc::new(parking_lot::Mutex::new(pre_approved));

        let tool_result = self
            .tool_executor
            .execute(
                tool_name,
                params.clone(),
                &self.session.id,
                &self.session.user,
                &approved,
                &self.recorder,
                Some(job_id),
            )
            .await;

        match tool_result {
            Ok(output) => {
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
                    ToolOutput::Error(e) => {
                        // Tool returned an error output — fall back to LLM
                        let diagnostic = format!(
                            "[cron:{job_id}] Tool '{tool_name}' returned error: {e}\n\
                             Diagnose the issue and report to the user.",
                        );
                        return self.dispatch_prompt(&diagnostic, "cron", job_id).await;
                    }
                };

                // Carry attachments through so a cron-scheduled SendFile actually
                // delivers the file — without this, the user would see the textual
                // confirmation but never receive the attachment.
                let mut content = vec![ContentBlock::Text(format!("[cron:{job_id}] {text}"))];
                content.extend(attachments);
                let response = OutgoingMessage {
                    session_id: self.session.id.clone(),
                    user_id: self.session.user.id.clone(),
                    channel: self.session.user.channel.clone(),
                    content,
                    reply_to: None,
                    metadata: MessageMetadata::default(),
                };
                if let Err(e) = self.response_tx.send(AgentOutput::Message(response)).await {
                    warn!(error = %e, "failed to send cron tool result to channel");
                }
                Ok(())
            }
            Err(e) => {
                // Execution failed — fall back to LLM for diagnosis
                warn!(
                    job_id = %job_id,
                    tool = %tool_name,
                    error = %e,
                    "cron tool call failed, falling back to LLM"
                );
                let diagnostic = format!(
                    "[cron:{job_id}] Tool '{tool_name}' failed with error: {e}\n\
                     Diagnose the issue and report to the user.",
                );
                self.dispatch_prompt(&diagnostic, "cron", job_id).await
            }
        }
    }

    /// Best-effort flush of trace data after each turn.
    async fn flush_trace(&self) {
        if let Err(e) = self.recorder.flush().await {
            warn!(
                session_id = %self.session.id,
                error = %e,
                "failed to flush trace after turn"
            );
        }
    }

    async fn handle_user_input(&mut self, incoming: IncomingMessage) -> anyhow::Result<()> {
        let message_clone = incoming.message.clone();
        let content = incoming.message.content;

        // Refresh the session's bot context from the most recent
        // inbound. Sidecars carry `bot_id` on every `Frame::Message`;
        // the session itself was created without one (the resolver
        // keys by channel + user, not bot). Treating the latest
        // message as authoritative for bot context lets multi-bot
        // MCP routing land the right `_meta.auraBotId` at tool
        // dispatch time without reshaping `ChannelSessionStore`.
        self.session.user.bot_id = incoming.message.sender.bot_id.clone();

        // PreMessage hook
        let mut hook_ctx = HookContext {
            session_id: self.session.id.clone(),
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
        let response = self
            .agent_loop
            .run(
                &mut self.session,
                content,
                &self.recorder,
                None,
                Some(self.response_tx.clone()),
            )
            .await?;

        // PreResponse hook
        let mut hook_ctx = HookContext {
            session_id: self.session.id.clone(),
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

        Ok(())
    }
}
