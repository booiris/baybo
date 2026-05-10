use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use aura_channels::{AgentOutput, Channel, ChannelRegistry, IncomingMessage, OutgoingMessage};
use aura_model::{BackgroundCompressionPayload, JobId, Session, SessionId, TriggerSource, User};

use aura_cron::CronTriggerEvent;

use crate::cost::CostManager;
use crate::security::SecurityGateway;
use crate::session::SessionManager;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::actor::AgentMessage;
use crate::supervisor::AgentSupervisor;

/// Request emitted by `AgentLoop`'s parent-side trigger gate and
/// consumed by `Router`'s `system_trigger_rx` arm. Replaces the old
/// `MaintenanceSpawner` trait: the gate just sends a value on an
/// `mpsc::Sender<SystemSpawnRequest>`; the router does the
/// session-create + actor-spawn + mailbox-dispatch.
///
/// `parent_actor_token` is the parent actor's lifetime token. The
/// router uses it as the new maintenance actor's `parent_token`, so
/// the maintenance child's `actor_token` derives as a grandchild —
/// cancelling the parent automatically cascades into the child via
/// the `tokio_util` token tree, with no explicit `Shutdown` mailbox
/// dance.
#[derive(Debug)]
pub enum SystemSpawnRequest {
    BackgroundCompression {
        parent_session_id: SessionId,
        parent_job_id: JobId,
        parent_actor_token: CancellationToken,
        payload: BackgroundCompressionPayload,
    },
}

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
/// token bridged to `ShutdownSignal`. For maintenance spawns the
/// originating parent actor's `actor_token` is passed (carried in the
/// `SystemSpawnRequest`), so cancelling the parent cascades into its
/// maintenance children automatically. For subagent dispatch the
/// parent's per-job cancel token is passed instead, so admin
/// `cancel_job(parent)` trips the entire descendant subtree.
pub type ActorSpawner = Box<
    dyn Fn(Session, mpsc::Sender<AgentOutput>, &CancellationToken) -> mpsc::Sender<AgentMessage>
        + Send
        + Sync,
>;

/// Routes incoming messages to the appropriate AgentActor.
pub struct Router {
    session_manager: Arc<SessionManager>,
    supervisor: AgentSupervisor,
    channels: Arc<ChannelRegistry>,
    security_gateway: Arc<SecurityGateway>,
    cost_manager: Arc<CostManager>,
    rate_limiter: RateLimiter,
    actor_spawner: ActorSpawner,
    /// Stored as `Option<Receiver>` so `run()` can `take()` them out of
    /// `self` to drive in `select!` arms; populated unconditionally
    /// from `RouterConfig` at construction.
    cron_trigger_rx: Option<mpsc::Receiver<CronTriggerEvent>>,
    system_trigger_rx: Option<mpsc::Receiver<SystemSpawnRequest>>,
    /// Cancellation parent passed to every top-level actor the router
    /// spawns. Bridged to the process-wide `ShutdownSignal` upstream;
    /// each actor derives its `actor_token` as a child of this so
    /// process shutdown cascades into every in-flight job.
    actor_parent_token: CancellationToken,
}

/// Construction bundle for [`Router`]. Every field is required — call
/// sites populate it via struct literal and pass to
/// [`Router::from_config`].
pub struct RouterConfig {
    pub session_manager: Arc<SessionManager>,
    pub supervisor: AgentSupervisor,
    pub channels: Arc<ChannelRegistry>,
    pub security_gateway: Arc<SecurityGateway>,
    pub cost_manager: Arc<CostManager>,
    pub actor_spawner: ActorSpawner,
    pub cron_trigger_rx: mpsc::Receiver<CronTriggerEvent>,
    pub system_trigger_rx: mpsc::Receiver<SystemSpawnRequest>,
    /// Cancellation parent passed to every top-level actor the router
    /// spawns. Bridged to the process-wide `ShutdownSignal` upstream.
    pub actor_parent_token: CancellationToken,
    /// Per-user sliding-window rate limit. Both fields are required
    /// (no default) — production wiring sources them from the user
    /// config; tests pass whatever they want.
    pub rate_limit_max_requests: usize,
    pub rate_limit_window: std::time::Duration,
}

impl Router {
    pub fn from_config(config: RouterConfig) -> Self {
        let RouterConfig {
            session_manager,
            supervisor,
            channels,
            security_gateway,
            cost_manager,
            actor_spawner,
            cron_trigger_rx,
            system_trigger_rx,
            actor_parent_token,
            rate_limit_max_requests,
            rate_limit_window,
        } = config;
        Self {
            session_manager,
            supervisor,
            channels,
            security_gateway,
            cost_manager,
            rate_limiter: RateLimiter::new(rate_limit_max_requests, rate_limit_window),
            actor_spawner,
            cron_trigger_rx: Some(cron_trigger_rx),
            system_trigger_rx: Some(system_trigger_rx),
            actor_parent_token,
        }
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
        let mut system_rx = self.system_trigger_rx.take();

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
                Some(request) = async {
                    match system_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Err(e) = self.handle_system_spawn(request).await {
                        error!(error = %e, "failed to handle system-spawn request");
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

    /// Materialise a [`SystemSpawnRequest`] into a maintenance session
    /// + actor + dispatched mailbox message.
    ///
    /// Symmetric to [`Self::handle_cron_trigger`] but with two
    /// task-specific knobs:
    ///   - **Session**: created via `create_maintenance_session` so the
    ///     row is `is_normal_session = 0` and lineaged to the parent.
    ///   - **Cancel parent**: the request carries the originating
    ///     parent actor's `actor_token`; the spawned maintenance
    ///     child's `actor_token` derives as a grandchild. Cancelling
    ///     the parent therefore cascades into the child via the
    ///     `tokio_util` token tree, with no Shutdown mailbox dance.
    ///
    /// The maintenance actor reuses the supervisor's `response_tx` for
    /// constructor-shape parity with user/cron actors. Its
    /// `handle_system_trigger` doesn't construct any `AgentOutput`, so
    /// nothing ever flows down it.
    ///
    /// As with cron, the spawned actor is not registered with the
    /// supervisor — maintenance is one-shot and registering would just
    /// accumulate dangling handles.
    async fn handle_system_spawn(&mut self, request: SystemSpawnRequest) -> anyhow::Result<()> {
        let SystemSpawnRequest::BackgroundCompression {
            parent_session_id,
            parent_job_id,
            parent_actor_token,
            payload,
        } = request;

        let parent = self
            .session_manager
            .get(&parent_session_id)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("parent session {parent_session_id} not found for summary spawn")
            })?;

        let maint = self
            .session_manager
            .create_maintenance_session(
                &parent,
                parent_job_id,
                aura_model::SystemReason::BackgroundCompression,
            )
            .await?;
        let maint_session_id = maint.id.clone();

        debug!(
            parent_session_id = %parent_session_id,
            maint_session_id = %maint_session_id,
            "routing system-spawn request to fresh maintenance session"
        );

        // `&parent_actor_token` becomes the cancel parent so the new
        // actor's `actor_token` is a grandchild of the originating
        // parent — Shutdown of the parent cascades automatically.
        let response_tx = self.supervisor.response_tx().clone();
        let mailbox = (self.actor_spawner)(maint, response_tx, &parent_actor_token);

        if let Err(e) = mailbox
            .send(AgentMessage::SystemTrigger(
                aura_model::SystemTrigger::BackgroundCompression(payload),
            ))
            .await
        {
            warn!(
                parent_session_id = %parent_session_id,
                maint_session_id = %maint_session_id,
                error = %e,
                "failed to deliver SystemTrigger to maintenance actor mailbox"
            );
        }
        Ok(())
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

        debug!(
            session_id = %session_id,
            job_id = %event.job_id,
            "routing cron trigger to fresh session"
        );

        let response_tx = self.supervisor.response_tx().clone();
        let sender = (self.actor_spawner)(session, response_tx, &self.actor_parent_token);

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
        self.cost_manager.check().map_err(|e| {
            warn!(
                user_id = %user.id,
                session_id = %session_id,
                error = %e,
                "cost manager rejected request"
            );
            anyhow::anyhow!(e)
        })?;

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
            info!(session_id = %session_id, "creating new actor for session");
            let response_tx = self.supervisor.response_tx().clone();
            let sender = (self.actor_spawner)(session, response_tx, &self.actor_parent_token);
            self.supervisor.register(session_id.clone(), sender);

            // Retry routing now that the actor exists.
            let re_routed = self
                .supervisor
                .route(&session_id, AgentMessage::UserInput(Box::new(incoming)))
                .await;
            if !re_routed {
                warn!(session_id = %session_id, "failed to route after actor creation");
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
