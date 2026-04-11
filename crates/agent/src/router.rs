use std::sync::Arc;

use aura_channels::{ChannelRegistry, IncomingMessage, OutgoingMessage};
use aura_session::{Session, User};

use crate::cron::CronTriggerEvent;
use crate::security::SecurityGateway;
use crate::session::SessionManager;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::actor::AgentMessage;
use crate::supervisor::AgentSupervisor;

/// A callback that creates and spawns a new AgentActor for a given session.
///
/// Returns the mailbox sender for communicating with the spawned actor.
/// The closure captures all dependencies needed to construct an actor
/// (AgentLoop, HookManager, ObservabilityRecorder, etc.).
pub type ActorSpawner =
    Box<dyn Fn(Session, mpsc::Sender<OutgoingMessage>) -> mpsc::Sender<AgentMessage> + Send + Sync>;

/// Routes incoming messages to the appropriate AgentActor.
pub struct Router {
    session_manager: SessionManager,
    supervisor: AgentSupervisor,
    channels: ChannelRegistry,
    security_gateway: Arc<SecurityGateway>,
    actor_spawner: Option<ActorSpawner>,
    cron_trigger_rx: Option<mpsc::Receiver<CronTriggerEvent>>,
}

impl Router {
    pub fn new(
        session_manager: SessionManager,
        supervisor: AgentSupervisor,
        channels: ChannelRegistry,
        security_gateway: Arc<SecurityGateway>,
    ) -> Self {
        Self {
            session_manager,
            supervisor,
            channels,
            security_gateway,
            actor_spawner: None,
            cron_trigger_rx: None,
        }
    }

    /// Set an actor spawner for on-demand actor creation.
    pub fn with_actor_spawner(mut self, spawner: ActorSpawner) -> Self {
        self.actor_spawner = Some(spawner);
        self
    }

    /// Set a receiver for cron trigger events.
    pub fn with_cron_triggers(mut self, rx: mpsc::Receiver<CronTriggerEvent>) -> Self {
        self.cron_trigger_rx = Some(rx);
        self
    }

    /// Start all channels and begin routing messages.
    pub async fn run(
        mut self,
        mut incoming_rx: mpsc::Receiver<IncomingMessage>,
        mut response_rx: mpsc::Receiver<OutgoingMessage>,
    ) {
        info!(channel_count = self.channels.len(), "router starting");

        let mut cron_rx = self.cron_trigger_rx.take();

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
                Some(event) = async {
                    match cron_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Err(e) = self.handle_cron_trigger(event).await {
                        error!(error = %e, "failed to handle cron trigger");
                    }
                }
                else => {
                    info!("router channels closed, shutting down");
                    break;
                }
            }
        }

        self.supervisor.shutdown_all().await;
    }

    /// Handle a cron trigger by resolving (or creating) a session for the
    /// target user+channel combination and routing a `CronTrigger` message.
    async fn handle_cron_trigger(&mut self, event: CronTriggerEvent) -> anyhow::Result<()> {
        // Stable session ID derived from user+channel so repeated cron
        // triggers reuse a single session for conversational continuity.
        let session_id = format!("cron-{}-{}", event.user_id, event.channel);

        let user = User {
            id: event.user_id.clone(),
            name: None,
            channel: event.channel,
        };

        debug!(
            session_id = %session_id,
            job_id = %event.job_id,
            "routing cron trigger"
        );

        let session = self
            .session_manager
            .get_or_create(&session_id, user, event.channel)
            .await?;

        self.session_manager.touch(&session_id).await?;

        let message = AgentMessage::CronTrigger {
            job_id: event.job_id.clone(),
            prompt: event.prompt,
        };

        let routed = self.supervisor.route(&session_id, message.clone()).await;

        if !routed {
            if let Some(ref spawner) = self.actor_spawner {
                info!(session_id = %session_id, "creating new actor for cron session");
                let response_tx = self.supervisor.response_tx().clone();
                let sender = spawner(session, response_tx);
                self.supervisor.register(session_id.clone(), sender);

                if !self.supervisor.route(&session_id, message).await {
                    warn!(session_id = %session_id, "failed to route cron trigger after actor creation");
                }
            } else {
                warn!(session_id = %session_id, "no actor spawner configured for cron trigger");
            }
        }

        Ok(())
    }

    async fn handle_incoming(&mut self, mut incoming: IncomingMessage) -> anyhow::Result<()> {
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
            return Err(e.into());
        }

        // Update last active time
        self.session_manager.touch(&session_id).await?;

        // Route to actor. If no actor exists and we have a spawner,
        // create one on-demand and retry.
        let routed = self
            .supervisor
            .route(
                &session_id,
                AgentMessage::UserInput(Box::new(incoming.clone())),
            )
            .await;

        if !routed {
            if let Some(ref spawner) = self.actor_spawner {
                info!(session_id = %session_id, "creating new actor for session");
                let response_tx = self.supervisor.response_tx().clone();
                let sender = spawner(session, response_tx);
                self.supervisor.register(session_id.clone(), sender);

                // Retry routing now that the actor exists.
                let re_routed = self
                    .supervisor
                    .route(&session_id, AgentMessage::UserInput(Box::new(incoming)))
                    .await;
                if !re_routed {
                    warn!(session_id = %session_id, "failed to route after actor creation");
                }
            } else {
                warn!(
                    session_id = %session_id,
                    "no actor found and no actor spawner configured"
                );
            }
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
        match self.channels.get(channel_type) {
            Some(adapter) => {
                if let Err(e) = adapter.send_response(outgoing).await {
                    error!(
                        channel = %channel_type,
                        error = %e,
                        "failed to send response through channel"
                    );
                }
            }
            None => {
                error!(
                    channel = %channel_type,
                    "no adapter found for channel type"
                );
            }
        }
    }
}
