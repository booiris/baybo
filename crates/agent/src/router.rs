use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use aura_channels::{AgentOutput, Channel, ChannelRegistry, IncomingMessage, OutgoingMessage};
use aura_model::{Session, SessionId, TriggerSource, User};

use aura_cron::CronTriggerEvent;

use crate::cost::CostManager;
use crate::security::SecurityGateway;
use crate::session::SessionManager;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
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
/// (AgentLoop, ToolExecutor, JobLifecycle, SpanRecorder, etc.).
///
/// `parent_token` is the cancellation parent the child actor's
/// `actor_token` is derived from. Tripping the parent cascades cancel
/// down through every job / tool / nested subagent the child runs. For
/// top-level user / cron sessions the router passes a process-wide
/// token bridged to `ShutdownSignal`. For subagent dispatch the parent's
/// per-job cancel token is passed instead, so admin `cancel_job(parent)`
/// trips the entire descendant subtree.
pub type ActorSpawner = Box<
    dyn Fn(
            Session,
            Vec<aura_model::ChatMessage>,
            mpsc::Sender<AgentOutput>,
            &CancellationToken,
        ) -> mpsc::Sender<AgentMessage>
        + Send
        + Sync,
>;

/// Routes incoming messages to the appropriate AgentActor.
pub struct Router {
    session_manager: Arc<SessionManager>,
    supervisor: AgentSupervisor,
    channels: Arc<ChannelRegistry>,
    security_gateway: Arc<SecurityGateway>,
    cost_manager: Option<Arc<CostManager>>,
    rate_limiter: RateLimiter,
    actor_spawner: Option<ActorSpawner>,
    cron_trigger_rx: Option<mpsc::Receiver<CronTriggerEvent>>,
    /// Cancellation parent passed to every top-level actor the router
    /// spawns. Bridged to the process-wide `ShutdownSignal` upstream;
    /// each actor derives its `actor_token` as a child of this so
    /// process shutdown cascades into every in-flight job. Defaults
    /// to a fresh standalone token if `with_actor_parent_token` was
    /// never called.
    actor_parent_token: CancellationToken,
}

/// Default rate limit: 30 requests per 60 seconds per user.
const DEFAULT_RATE_LIMIT_REQUESTS: usize = 30;
const DEFAULT_RATE_LIMIT_WINDOW_SECS: u64 = 60;

impl Router {
    pub fn new(
        session_manager: Arc<SessionManager>,
        supervisor: AgentSupervisor,
        channels: Arc<ChannelRegistry>,
        security_gateway: Arc<SecurityGateway>,
    ) -> Self {
        Self {
            session_manager,
            supervisor,
            channels,
            security_gateway,
            cost_manager: None,
            rate_limiter: RateLimiter::new(
                DEFAULT_RATE_LIMIT_REQUESTS,
                std::time::Duration::from_secs(DEFAULT_RATE_LIMIT_WINDOW_SECS),
            ),
            actor_spawner: None,
            cron_trigger_rx: None,
            actor_parent_token: CancellationToken::new(),
        }
    }

    /// Set the cancellation parent passed to every top-level actor the
    /// router spawns. Wired upstream to `ShutdownSignal` so process
    /// shutdown cascades into every in-flight job. Without this, the
    /// router uses a fresh standalone token and shutdown does not
    /// reach in-flight LLM / tool calls.
    pub fn with_actor_parent_token(mut self, token: CancellationToken) -> Self {
        self.actor_parent_token = token;
        self
    }

    /// Attach the [`CostManager`] so the router can pre-flight reject
    /// over-budget messages before they enter an actor — same gate the
    /// agent loop uses before each LLM call, just at message ingress.
    pub fn with_cost_manager(mut self, manager: Arc<CostManager>) -> Self {
        self.cost_manager = Some(manager);
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
        mut response_rx: mpsc::Receiver<AgentOutput>,
    ) {
        let channel_count = self.channels.len();
        info!(channel_count, "router starting");

        let mut cron_rx = self.cron_trigger_rx.take();

        loop {
            tokio::select! {
                Some(incoming) = incoming_rx.recv() => {
                    if let Err(e) = self.handle_incoming(incoming).await {
                        error!(error = %e, "failed to handle incoming message");
                    }
                }
                Some(output) = response_rx.recv() => {
                    self.handle_agent_output(output).await;
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

    /// Handle a cron trigger by minting a fresh session and routing a
    /// `CronTrigger` message into a one-shot actor.
    ///
    /// Each fire creates an isolated session so the trigger sees a
    /// clean transcript and a fresh `SessionState` (no leaked
    /// `approved_resources`, `active_skills`, or compression state
    /// from prior fires). Continuity across fires belongs to memory +
    /// skill loading, not to a shared mutable transcript.
    ///
    /// The spawned actor is intentionally NOT registered with the
    /// supervisor: each cron session is one-shot and has no follow-up
    /// traffic, so registering would just accumulate dangling actor
    /// handles in the supervisor's map. We send `CronTrigger` followed
    /// by `Shutdown`; the actor processes the trigger (FIFO), exits on
    /// Shutdown, and its mailbox closes when this function returns and
    /// drops the sender.
    async fn handle_cron_trigger(&mut self, event: CronTriggerEvent) -> anyhow::Result<()> {
        let user = User {
            id: event.user_id.clone(),
            name: None,
            channel: event.channel.clone(),
        };

        let session = self
            .session_manager
            .create_session_with_trigger(
                user,
                event.channel.clone(),
                TriggerSource::Cron {
                    cron_job_id: event.job_id.clone(),
                },
            )
            .await?;
        let session_id = session.id.clone();

        let Some(ref spawner) = self.actor_spawner else {
            warn!(session_id = %session_id, "no actor spawner configured for cron trigger");
            return Ok(());
        };

        debug!(
            session_id = %session_id,
            job_id = %event.job_id,
            "routing cron trigger to fresh session"
        );

        let response_tx = self.supervisor.response_tx().clone();
        // Cron sessions are minted fresh per fire (`Lineage::Subagent`-
        // adjacent), so they have no prior transcript to seed.
        let sender = spawner(session, Vec::new(), response_tx, &self.actor_parent_token);

        let trigger_msg = AgentMessage::CronTrigger {
            job_id: event.job_id.clone(),
            prompt: event.prompt,
        };
        if let Err(e) = sender.send(trigger_msg).await {
            warn!(session_id = %session_id, error = %e, "failed to deliver cron trigger");
            return Ok(());
        }
        if let Err(e) = sender.send(AgentMessage::Shutdown).await {
            warn!(
                session_id = %session_id,
                error = %e,
                "failed to deliver post-trigger shutdown; actor will still exit when sender drops",
            );
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
        let channel = incoming.message.channel.clone();

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

        // In-memory budget gate: same call agent_loop makes before
        // each LLM call, fired here too so an over-cap user never even
        // gets an actor spun up.
        if let Some(ref cm) = self.cost_manager {
            cm.check().map_err(|e| {
                warn!(
                    user_id = %user.id,
                    session_id = %session_id,
                    error = %e,
                    "cost manager rejected request"
                );
                anyhow::anyhow!(e)
            })?;
        }

        // Get or create session
        let typed_session_id = SessionId::from(session_id.as_str());
        let mut session = self
            .session_manager
            .get_or_create(&typed_session_id, user, channel)
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
        self.session_manager.touch(&typed_session_id).await?;

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
                // Cold-start path: load any persisted transcript so a
                // process bounce / actor respawn doesn't drop the
                // user's earlier turns. Empty on first contact.
                let transcript = self
                    .session_manager
                    .load_context_messages(&typed_session_id)
                    .await
                    .unwrap_or_else(|e| {
                        warn!(
                            session_id = %session_id,
                            error = %e,
                            "failed to load persisted transcript; starting fresh"
                        );
                        Vec::new()
                    });
                let sender = spawner(session, transcript, response_tx, &self.actor_parent_token);
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

    async fn handle_agent_output(&self, output: AgentOutput) {
        let (session_id, channel) = match &output {
            AgentOutput::Delta {
                session_id,
                channel,
                ..
            }
            | AgentOutput::Notice {
                session_id,
                channel,
                ..
            } => (session_id.clone(), channel.clone()),
            AgentOutput::Message(outgoing) => {
                (outgoing.session_id.clone(), outgoing.channel.clone())
            }
        };

        // `Message` is the only variant that carries user-visible prose
        // subject to policy egress — sanitize it in place before dispatch.
        // `Delta` chunks are intentionally exempt (incremental streaming;
        // the final `Message` is the authoritative sanitized egress per
        // `docs/modules/security.md`), and `Notice` is system-authored.
        let output = match output {
            AgentOutput::Message(outgoing) => {
                AgentOutput::Message(self.sanitize_outgoing(outgoing).await)
            }
            other => other,
        };

        let Some(channel_handle) = self.channels.get_for(&channel, &session_id) else {
            debug!(
                channel = %channel,
                session_id = %session_id,
                "no channel registered for agent output"
            );
            return;
        };

        self.send_to_channel(channel_handle, output, session_id, channel)
            .await;
    }

    /// Run the security gateway over an outgoing message. Returns the
    /// (possibly mutated) message — if the session vanished or the gateway
    /// errored, the message is forwarded as-is; the gateway mutates in
    /// place even on error (redaction notice replaces the content) so we
    /// still send what it produced rather than the original.
    async fn sanitize_outgoing(&self, mut outgoing: OutgoingMessage) -> OutgoingMessage {
        let span = tracing::info_span!(
            "sanitize_outgoing",
            session_id = %outgoing.session_id,
            channel = %outgoing.channel,
        );
        let _guard = span.enter();

        let outgoing_session_id = SessionId::from(outgoing.session_id.as_str());
        let session = match self.session_manager.get(&outgoing_session_id).await {
            Ok(Some(s)) => s,
            Ok(None) => {
                warn!(
                    session_id = %outgoing.session_id,
                    "session not found for outgoing sanitization, skipping security scan"
                );
                return outgoing;
            }
            Err(e) => {
                error!(
                    session_id = %outgoing.session_id,
                    error = %e,
                    "failed to load session for output sanitization"
                );
                return outgoing;
            }
        };

        if let Err(e) = self
            .security_gateway
            .sanitize_output(&mut outgoing, &session)
            .await
        {
            warn!(
                session_id = %outgoing.session_id,
                error = %e,
                "security gateway blocked or modified outgoing message"
            );
        }
        outgoing
    }

    async fn send_to_channel(
        &self,
        channel_handle: Arc<Channel>,
        output: AgentOutput,
        session_id: String,
        channel: aura_model::ChannelType,
    ) {
        if let Err(e) = channel_handle.send(output).await {
            error!(
                channel = %channel,
                session_id = %session_id,
                error = %e,
                "failed to deliver agent output"
            );
        }
    }
}
