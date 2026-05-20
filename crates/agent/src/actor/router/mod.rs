//! Routes inbound messages, cron fires, and system-spawn requests
//! to per-session [`AgentActor`](crate::actor::AgentActor) instances,
//! and fans actor-emitted [`AgentOutput`] back out through registered
//! channels.
//!
//! Each handler lives in its own submodule:
//! - [`user_input`] — user input from channel sidecars
//! - [`cron`] — scheduled cron-trigger events
//! - [`system_spawn`] — system-initiated actor spawns, with one
//!   submodule per [`aura_model::SystemSpawnRequest`] variant
//! - [`output`] — actor → channel fan-out + egress sanitization
//!
//! The `Router` struct itself, its `select!`-driven [`Router::run`]
//! loop, and the one-shot actor spawn helper used by the cron /
//! system_spawn paths live here in [`mod@self`].

mod cron;
mod output;
mod system_spawn;
mod user_input;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use aura_channels::{AgentOutput, ChannelRegistry, IncomingMessage};
use aura_cron::CronTriggerEvent;
use aura_model::{Session, SystemSpawnRequest};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::actor::AgentMessage;
use crate::actor::supervisor::AgentSupervisor;
use crate::security::SecurityGateway;
use aura_cost::CostManager;
use aura_job::JobLifecycle;
use aura_session::SessionManager;

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

/// Builds and spawns an [`AgentActor`] for `session`, returning its
/// mailbox. `actor_token` is installed as the actor's
/// `VolatileResources::actor_token` — callers that need to cancel
/// the spawned actor (e.g. the subagent waiter on timeout) must
/// keep their own clone before calling.
///
/// [`AgentActor`]: crate::actor::AgentActor
pub type ActorSpawner = Box<
    dyn Fn(
            Session,
            /* initial_llm */ Option<String>,
            mpsc::Sender<AgentOutput>,
            /* actor_token */ CancellationToken,
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
    cost_manager: Arc<CostManager>,
    rate_limiter: RateLimiter,
    actor_spawner: ActorSpawner,
    /// Job lifecycle handle. Needed to subscribe to terminal-event
    /// broadcasts when waiting on a subagent child to terminate, and
    /// to reconcile via the store on broadcast lag.
    job_lifecycle: Arc<JobLifecycle>,
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
    /// External-agent backends registered at boot.
    pub(crate) external_agents: Arc<crate::external_agent::ExternalAgentRegistry>,
    /// Workspace paths; used to compute external-agent working dirs
    /// (`<root>/work/<kind>/<name-or-session>/`).
    pub(crate) workspace_paths: Arc<aura_workspace::WorkspacePaths>,
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
    pub job_lifecycle: Arc<JobLifecycle>,
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
    pub external_agents: Arc<crate::external_agent::ExternalAgentRegistry>,
    pub workspace_paths: Arc<aura_workspace::WorkspacePaths>,
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
            job_lifecycle,
            cron_trigger_rx,
            system_trigger_rx,
            actor_parent_token,
            rate_limit_max_requests,
            rate_limit_window,
            external_agents,
            workspace_paths,
        } = config;
        Self {
            session_manager,
            supervisor,
            channels,
            security_gateway,
            cost_manager,
            rate_limiter: RateLimiter::new(rate_limit_max_requests, rate_limit_window),
            actor_spawner,
            job_lifecycle,
            cron_trigger_rx: Some(cron_trigger_rx),
            system_trigger_rx: Some(system_trigger_rx),
            actor_parent_token,
            external_agents,
            workspace_paths,
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

    /// Spawn a **one-shot** actor: build the mailbox + actor_token, but
    /// do NOT register the handle with the supervisor. Used by cron,
    /// maintenance, and subagent spawns where the session has no
    /// follow-up traffic and registering would just accumulate dangling
    /// handles. Callers drive shutdown via `Shutdown` mailbox dispatch,
    /// `actor_token` cancellation, or by dropping the sender so the
    /// mailbox closes naturally.
    ///
    /// The persistent counterpart for user sessions lives in
    /// [`AgentSupervisor::route_or_spawn`], which folds the build +
    /// atomic register into one call.
    fn spawn_oneshot_actor(
        &self,
        session: Session,
        initial_llm: Option<String>,
        response_tx: mpsc::Sender<AgentOutput>,
        parent_token: &CancellationToken,
    ) -> (mpsc::Sender<AgentMessage>, CancellationToken) {
        let actor_token = parent_token.child_token();
        let mailbox = (self.actor_spawner)(session, initial_llm, response_tx, actor_token.clone());
        (mailbox, actor_token)
    }
}
