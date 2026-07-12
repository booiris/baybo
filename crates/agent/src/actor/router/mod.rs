//! Routes inbound messages and cron fires to per-session
//! [`AgentActor`](crate::actor::AgentActor) instances, and fans
//! actor-emitted [`AgentOutput`] back out through registered channels.
//!
//! Each handler lives in its own submodule:
//! - [`user_input`] — user input from channel sidecars
//! - [`cron`] — scheduled cron-trigger events
//! - [`output`] — actor → channel fan-out + egress sanitization
//!
//! The `Router` struct itself, its `select!`-driven [`Router::run`]
//! loop, and the one-shot actor spawn helper ([`build_oneshot_actor`])
//! used by the cron path live here in [`mod@self`]. Subagent spawns are
//! handled out-of-band by [`crate::runtime::subagent_spawner`], which the
//! `spawn_subagent` tool calls directly.

mod cron;
mod output;
mod user_input;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use baybo_channels::{AgentOutput, ChannelRegistry, RouterInbound};
use baybo_cron::CronTriggerEvent;
use baybo_model::{LlmEntryName, Session};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::actor::AgentMessage;
use crate::actor::mailbox::MailboxSender;
use crate::actor::supervisor::AgentSupervisor;
use crate::security::SecurityGateway;
use baybo_cost::CostManager;
use baybo_job::JobLifecycle;
use baybo_session::SessionManager;
use baybo_store::CronStore;

/// Live, atomically-updatable rate-limit knobs, shared between the
/// `Router`'s [`RateLimiter`] (reader) and the config-reload
/// `CostReloader` (writer). Only the two config values are shared; the
/// per-user timestamp map stays owned by the `RateLimiter`. `Relaxed`
/// ordering is fine — the two knobs are independent and a check that
/// races a reload sees the old or the new value, never a torn one.
pub struct LiveRateLimit {
    max_requests: AtomicUsize,
    window_secs: AtomicU64,
}

impl LiveRateLimit {
    pub fn new(max_requests: usize, window: std::time::Duration) -> Arc<Self> {
        Arc::new(Self {
            max_requests: AtomicUsize::new(max_requests),
            window_secs: AtomicU64::new(window.as_secs()),
        })
    }

    /// Swap both knobs live (config hot-reload). The next `check` sees them.
    pub fn set(&self, max_requests: usize, window: std::time::Duration) {
        self.max_requests.store(max_requests, Ordering::Relaxed);
        self.window_secs.store(window.as_secs(), Ordering::Relaxed);
    }

    fn max_requests(&self) -> usize {
        self.max_requests.load(Ordering::Relaxed)
    }

    fn window(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.window_secs.load(Ordering::Relaxed))
    }
}

/// Per-user sliding-window rate limiter.
///
/// Tracks timestamps of recent requests per user and rejects requests that
/// exceed the limit within the window. Both limits are read live from a
/// shared [`LiveRateLimit`] so a `cost.rate_limit` config reload takes
/// effect on the next request without rebuilding the `Router`.
pub(crate) struct RateLimiter {
    limits: Arc<LiveRateLimit>,
    /// Per-user request timestamps.
    requests: HashMap<String, Vec<Instant>>,
}

impl RateLimiter {
    pub(crate) fn new(limits: Arc<LiveRateLimit>) -> Self {
        Self {
            limits,
            requests: HashMap::new(),
        }
    }

    /// Returns `true` if the request is allowed, `false` if rate-limited.
    pub(crate) fn check(&mut self, user_id: &str) -> bool {
        let now = Instant::now();
        let window = self.limits.window();
        let max_requests = self.limits.max_requests();
        let timestamps = self.requests.entry(user_id.to_string()).or_default();

        // Evict entries outside the window.
        timestamps.retain(|&t| now.duration_since(t) < window);

        if timestamps.len() >= max_requests {
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
/// The child's system prompt is resolved by its `ContextManager` from
/// `session.state.subagent_type` (the profile name, set at spawn) via the
/// subagent profile registry; a `None` subagent_type yields the workspace
/// soul. So the spawner needs no prompt argument — the `Session` carries it.
///
/// [`AgentActor`]: crate::actor::AgentActor
pub type ActorSpawner = Arc<
    dyn Fn(
            Session,
            /* initial_llm */ Option<LlmEntryName>,
            mpsc::Sender<AgentOutput>,
            /* actor_token */ CancellationToken,
        ) -> MailboxSender<AgentMessage>
        + Send
        + Sync,
>;

/// Build a one-shot actor: derive a child `actor_token` from
/// `parent_token`, hand the session to `actor_spawner` (which spawns the
/// actor's run loop on its own task), and return the mailbox + token.
/// Shared by the router's cron path and the subagent spawner — neither
/// registers the handle with the supervisor (one-shot sessions have no
/// follow-up traffic, so registering would just accumulate dangling
/// handles). The persistent counterpart for user sessions lives in
/// [`AgentSupervisor::route_or_spawn`].
pub(crate) fn build_oneshot_actor(
    actor_spawner: &ActorSpawner,
    parent_token: &CancellationToken,
    session: Session,
    initial_llm: Option<LlmEntryName>,
    response_tx: mpsc::Sender<AgentOutput>,
) -> (MailboxSender<AgentMessage>, CancellationToken) {
    let actor_token = parent_token.child_token();
    let mailbox = actor_spawner(session, initial_llm, response_tx, actor_token.clone());
    (mailbox, actor_token)
}

/// Routes incoming messages to the appropriate AgentActor.
pub struct Router {
    session_manager: Arc<SessionManager>,
    supervisor: AgentSupervisor,
    channels: Arc<ChannelRegistry>,
    security_gateway: Arc<SecurityGateway>,
    cost_manager: Arc<CostManager>,
    rate_limiter: RateLimiter,
    actor_spawner: ActorSpawner,
    /// Job lifecycle handle — subscribe to terminal-event broadcasts and
    /// reconcile via the store on broadcast lag.
    job_lifecycle: Arc<JobLifecycle>,
    /// Delivery ledger for one-shot cron results: the cron waiter stamps a
    /// fire's outcome here, and `run()` scans it at boot to re-drive results
    /// that never reached their conversation.
    cron_store: Arc<dyn CronStore>,
    /// Stored as `Option<Receiver>` so `run()` can `take()` it out of
    /// `self` to drive in a `select!` arm; populated unconditionally from
    /// `RouterConfig` at construction.
    cron_trigger_rx: Option<mpsc::Receiver<CronTriggerEvent>>,
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
    pub job_lifecycle: Arc<JobLifecycle>,
    /// Delivery ledger for one-shot cron results — see [`Router::cron_store`].
    pub cron_store: Arc<dyn CronStore>,
    pub cron_trigger_rx: mpsc::Receiver<CronTriggerEvent>,
    /// Cancellation parent passed to every top-level actor the router
    /// spawns. Bridged to the process-wide `ShutdownSignal` upstream.
    pub actor_parent_token: CancellationToken,
    /// Per-user sliding-window rate limit, shared live with the config
    /// reloader so `cost.rate_limit` edits take effect without
    /// rebuilding the router. Production wiring sources it from config;
    /// tests build one from whatever values they want.
    pub rate_limit: Arc<LiveRateLimit>,
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
            cron_store,
            cron_trigger_rx,
            actor_parent_token,
            rate_limit,
        } = config;
        Self {
            session_manager,
            supervisor,
            channels,
            security_gateway,
            cost_manager,
            rate_limiter: RateLimiter::new(rate_limit),
            actor_spawner,
            job_lifecycle,
            cron_store,
            cron_trigger_rx: Some(cron_trigger_rx),
            actor_parent_token,
        }
    }

    /// Start all channels and begin routing messages.
    pub async fn run(
        mut self,
        mut incoming_rx: mpsc::Receiver<RouterInbound>,
        mut response_rx: mpsc::Receiver<AgentOutput>,
    ) {
        let channel_count = self.channels.len();
        info!(channel_count, "router starting");

        // Before any live traffic: hand over one-shot cron results whose
        // delivery a crash interrupted. Idempotent — an origin that already
        // has the result drops the replay.
        self.redrive_cron_deliveries().await;

        let mut cron_rx = self.cron_trigger_rx.take();

        loop {
            tokio::select! {
                Some(inbound) = incoming_rx.recv() => {
                    match inbound {
                        RouterInbound::One(incoming) => {
                            if let Err(e) = self.handle_incoming(*incoming).await {
                                error!(error = %e, "failed to handle incoming message");
                            }
                        }
                        RouterInbound::Batch(batch) => {
                            if let Err(e) = self.handle_incoming_batch(batch).await {
                                error!(error = %e, "failed to handle incoming batch");
                            }
                        }
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

    /// Thin instance wrapper over [`build_oneshot_actor`] for the cron
    /// path, which has `&self` in hand.
    fn spawn_oneshot_actor(
        &self,
        session: Session,
        initial_llm: Option<LlmEntryName>,
        response_tx: mpsc::Sender<AgentOutput>,
        parent_token: &CancellationToken,
    ) -> (MailboxSender<AgentMessage>, CancellationToken) {
        build_oneshot_actor(
            &self.actor_spawner,
            parent_token,
            session,
            initial_llm,
            response_tx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_rate_limit_swap_changes_cap() {
        let limit = LiveRateLimit::new(2, std::time::Duration::from_secs(60));
        let mut limiter = RateLimiter::new(Arc::clone(&limit));
        assert!(limiter.check("u"));
        assert!(limiter.check("u"));
        assert!(!limiter.check("u"), "third request exceeds the cap of 2");

        // Raise the cap live (config hot-reload path); the next request
        // is admitted without rebuilding the limiter.
        limit.set(10, std::time::Duration::from_secs(60));
        assert!(limiter.check("u"), "swapped-in cap must be seen by check");
    }
}
