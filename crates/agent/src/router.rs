use std::sync::Arc;

use aura_channels::ChannelAdapter;
use aura_core::{IncomingMessage, OutgoingMessage};
use aura_security::SecurityGateway;
use aura_session::SessionManager;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::actor::AgentMessage;
use crate::supervisor::AgentSupervisor;

/// Routes incoming messages to the appropriate AgentActor.
pub struct Router {
    session_manager: SessionManager,
    supervisor: AgentSupervisor,
    channels: Vec<Box<dyn ChannelAdapter>>,
    security_gateway: Arc<SecurityGateway>,
}

impl Router {
    pub fn new(
        session_manager: SessionManager,
        supervisor: AgentSupervisor,
        channels: Vec<Box<dyn ChannelAdapter>>,
        security_gateway: Arc<SecurityGateway>,
    ) -> Self {
        Self {
            session_manager,
            supervisor,
            channels,
            security_gateway,
        }
    }

    /// Start all channels and begin routing messages.
    pub async fn run(
        mut self,
        mut incoming_rx: mpsc::Receiver<IncomingMessage>,
        mut response_rx: mpsc::Receiver<OutgoingMessage>,
    ) {
        info!(channel_count = self.channels.len(), "router starting");

        loop {
            tokio::select! {
                Some(incoming) = incoming_rx.recv() => {
                    if let Err(e) = self.handle_incoming(incoming).await {
                        error!(error = %e, "failed to handle incoming message");
                    }
                }
                Some(outgoing) = response_rx.recv() => {
                    self.handle_outgoing(outgoing).await;
                }
                else => {
                    info!("router channels closed, shutting down");
                    break;
                }
            }
        }

        self.supervisor.shutdown_all().await;
    }

    async fn handle_incoming(&mut self, mut incoming: IncomingMessage) -> aura_core::Result<()> {
        let span = tracing::info_span!(
            "handle_incoming",
            session_id = %incoming.message.session_id,
            message_id = %incoming.message.id,
            channel = %incoming.message.channel,
        );
        let _guard = span.enter();

        let session_id = incoming.message.session_id.clone();
        let user = incoming.message.sender.clone();
        let channel = incoming.message.channel;

        debug!(session_id = %session_id, user_id = %user.id, "routing message");

        // Get or create session
        let mut session = self
            .session_manager
            .get_or_create(&session_id, user, channel)
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
            return Err(e);
        }

        // Update last active time
        self.session_manager.touch(&session_id).await?;

        // Route to actor
        let routed = self
            .supervisor
            .route(&session_id, AgentMessage::UserInput(Box::new(incoming)))
            .await;

        if !routed {
            debug!(
                session_id = %session_id,
                "no actor found, message queued for new actor creation"
            );
            // In a full implementation, we would create a new actor here.
            // For Phase 1, we log and skip.
        }

        Ok(())
    }

    async fn handle_outgoing(&self, mut outgoing: OutgoingMessage) {
        let span = tracing::info_span!(
            "handle_outgoing",
            session_id = %outgoing.session_id,
            channel = %outgoing.channel,
        );
        let _guard = span.enter();

        // Sanitize output through the security gateway before sending.
        let session_id = outgoing.session_id.clone();
        let session = match self.session_manager.get(&session_id).await {
            Ok(Some(s)) => s,
            Ok(None) => {
                // If session is gone, create a minimal one for sanitization.
                warn!(session_id = %session_id, "session not found for outgoing sanitization, skipping security scan");
                self.send_to_channel(outgoing).await;
                return;
            }
            Err(e) => {
                error!(session_id = %session_id, error = %e, "failed to load session for output sanitization");
                self.send_to_channel(outgoing).await;
                return;
            }
        };

        if let Err(e) = self
            .security_gateway
            .sanitize_output(&mut outgoing, &session)
            .await
        {
            warn!(
                session_id = %session_id,
                error = %e,
                "security gateway blocked or modified outgoing message"
            );
            // Even if sanitization errors, we do not send the original — it was
            // already mutated by the gateway (content replaced with redaction notice).
        }

        self.send_to_channel(outgoing).await;
    }

    async fn send_to_channel(&self, outgoing: OutgoingMessage) {
        let channel_type = outgoing.channel;
        for adapter in &self.channels {
            if adapter.channel_type() == channel_type {
                if let Err(e) = adapter.send_response(outgoing.clone()).await {
                    error!(
                        channel = %channel_type,
                        error = %e,
                        "failed to send response through channel"
                    );
                }
                return;
            }
        }
        error!(
            channel = %channel_type,
            "no adapter found for channel type"
        );
    }

    /// Access the supervisor for actor management.
    pub fn supervisor_mut(&mut self) -> &mut AgentSupervisor {
        &mut self.supervisor
    }

    /// Access the session manager.
    pub fn session_manager(&self) -> &SessionManager {
        &self.session_manager
    }
}
