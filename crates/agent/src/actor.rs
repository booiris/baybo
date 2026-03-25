use std::sync::Arc;

use aura_core::{IncomingMessage, OutgoingMessage, Session};
use aura_hook::{HookContext, HookManager, HookPoint};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::agent_loop::AgentLoop;
use crate::observability::ObservabilityRecorder;

/// Messages that can be sent to an AgentActor.
#[derive(Debug, Clone)]
pub enum AgentMessage {
    /// A user sent a message.
    UserInput(Box<IncomingMessage>),
    /// A cron job fired.
    CronTrigger { job_id: String },
    /// A heartbeat tick arrived.
    HeartbeatTick,
    /// A routine fired.
    RoutineTrigger { routine_id: String },
    /// Gracefully shut down this actor.
    Shutdown,
}

/// One actor per session. Receives messages sequentially through its mailbox.
pub struct AgentActor {
    session: Session,
    agent_loop: AgentLoop,
    response_tx: mpsc::Sender<OutgoingMessage>,
    hooks: Arc<HookManager>,
    recorder: Arc<ObservabilityRecorder>,
}

impl AgentActor {
    pub fn new(
        session: Session,
        agent_loop: AgentLoop,
        response_tx: mpsc::Sender<OutgoingMessage>,
        hooks: Arc<HookManager>,
        recorder: Arc<ObservabilityRecorder>,
    ) -> Self {
        Self {
            session,
            agent_loop,
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
                }
                AgentMessage::CronTrigger { job_id } => {
                    debug!(session_id = %self.session.id, job_id = %job_id, "received cron trigger");
                    // TODO: dispatch cron job prompt through agent loop
                }
                AgentMessage::HeartbeatTick => {
                    debug!(session_id = %self.session.id, "received heartbeat tick");
                    // TODO: process heartbeat (e.g. flush state, check health)
                }
                AgentMessage::RoutineTrigger { routine_id } => {
                    debug!(session_id = %self.session.id, routine_id = %routine_id, "received routine trigger");
                    // TODO: dispatch routine prompt through agent loop
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

    async fn handle_user_input(&mut self, incoming: IncomingMessage) -> aura_core::Result<()> {
        let message_clone = incoming.message.clone();
        let content = incoming.message.content;

        // PreMessage hook
        let mut hook_ctx = HookContext {
            session_id: self.session.id.clone(),
            user_id: Some(self.session.user.id.clone()),
            message: Some(message_clone),
            response: None,
            job_id: None,
            trace_span_id: None,
            extra: Default::default(),
        };
        self.hooks
            .trigger(HookPoint::PreMessage, &mut hook_ctx)
            .await?;

        // Run the agent loop
        let response = self
            .agent_loop
            .run(&mut self.session, content, &self.recorder, None)
            .await?;

        // PreResponse hook
        let mut hook_ctx = HookContext {
            session_id: self.session.id.clone(),
            user_id: Some(self.session.user.id.clone()),
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
        if let Err(e) = self.response_tx.send(response).await {
            warn!(error = %e, "failed to send response to channel");
        }

        Ok(())
    }
}
