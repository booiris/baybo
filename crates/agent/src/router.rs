use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use aura_channels::{ChannelRegistry, IncomingMessage, OutgoingMessage};
use aura_session::{Session, User};

use crate::cost::CostGuard;
use crate::cron::CronTriggerEvent;
use crate::security::SecurityGateway;
use crate::session::SessionManager;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, error, info, warn};

use crate::actor::AgentMessage;
use crate::supervisor::AgentSupervisor;

/// Per-user sliding-window rate limiter.
///
/// Tracks timestamps of recent requests per user and rejects requests that
/// exceed the configured limit within the window.
pub(crate) struct RateLimiter {
    /// Maximum requests allowed within the window.
    max_requests: usize,
    /// Sliding window duration.
    window: std::time::Duration,
    /// Per-user request timestamps.
    requests: HashMap<String, Vec<Instant>>,
}

impl RateLimiter {
    pub(crate) fn new(max_requests: usize, window: std::time::Duration) -> Self {
        Self {
            max_requests,
            window,
            requests: HashMap::new(),
        }
    }

    /// Returns `true` if the request is allowed, `false` if rate-limited.
    pub(crate) fn check(&mut self, user_id: &str) -> bool {
        let now = Instant::now();
        let timestamps = self.requests.entry(user_id.to_string()).or_default();

        // Evict entries outside the window.
        timestamps.retain(|&t| now.duration_since(t) < self.window);

        if timestamps.len() >= self.max_requests {
            return false;
        }

        timestamps.push(now);
        true
    }
}

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
    channels: Arc<RwLock<ChannelRegistry>>,
    security_gateway: Arc<SecurityGateway>,
    cost_guard: Option<Arc<CostGuard>>,
    rate_limiter: RateLimiter,
    actor_spawner: Option<ActorSpawner>,
    cron_trigger_rx: Option<mpsc::Receiver<CronTriggerEvent>>,
}

/// Default rate limit: 30 requests per 60 seconds per user.
const DEFAULT_RATE_LIMIT_REQUESTS: usize = 30;
const DEFAULT_RATE_LIMIT_WINDOW_SECS: u64 = 60;

impl Router {
    pub fn new(
        session_manager: SessionManager,
        supervisor: AgentSupervisor,
        channels: Arc<RwLock<ChannelRegistry>>,
        security_gateway: Arc<SecurityGateway>,
    ) -> Self {
        Self {
            session_manager,
            supervisor,
            channels,
            security_gateway,
            cost_guard: None,
            rate_limiter: RateLimiter::new(
                DEFAULT_RATE_LIMIT_REQUESTS,
                std::time::Duration::from_secs(DEFAULT_RATE_LIMIT_WINDOW_SECS),
            ),
            actor_spawner: None,
            cron_trigger_rx: None,
        }
    }

    /// Set a `CostGuard` for quota checks before routing messages.
    pub fn with_cost_guard(mut self, guard: Arc<CostGuard>) -> Self {
        self.cost_guard = Some(guard);
        self
    }

    /// Override the default rate limiter settings.
    pub fn with_rate_limit(mut self, max_requests: usize, window: std::time::Duration) -> Self {
        self.rate_limiter = RateLimiter::new(max_requests, window);
        self
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
        let channel_count = self.channels.read().await.len();
        info!(channel_count, "router starting");

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

        // User-level rate limiting
        if !self.rate_limiter.check(&user.id) {
            warn!(
                user_id = %user.id,
                session_id = %session_id,
                "user rate-limited"
            );
            anyhow::bail!("rate limit exceeded for user '{}'", user.id);
        }

        // Quota check via CostGuard
        if let Some(ref guard) = self.cost_guard {
            guard.check_quota(&user.id).await.map_err(|e| {
                warn!(
                    user_id = %user.id,
                    session_id = %session_id,
                    error = %e,
                    "cost guard rejected request"
                );
                anyhow::anyhow!(e)
            })?;
        }

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
        let channels = self.channels.read().await;
        match channels.get(channel_type) {
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
