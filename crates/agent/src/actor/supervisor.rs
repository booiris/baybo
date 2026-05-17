use std::collections::HashSet;
use std::sync::Arc;

use aura_channels::AgentOutput;
use aura_model::SessionId;
use chrono::Duration;
use dashmap::DashMap;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::actor::AgentMessage;
use aura_session::SessionManager;

/// Handle to communicate with a running AgentActor.
pub struct ActorHandle {
    pub sender: mpsc::Sender<AgentMessage>,
}

/// Manages AgentActor instances, one per active session.
///
/// Cheap to clone: the actor registry is a [`DashMap`] behind an `Arc`,
/// so router, runtime, and individual actors can share the same view
/// with sharded-lock concurrency (no global mutex held across reads).
/// Each actor holds a clone and uses it to self-deregister on shutdown
/// via [`ActorRegistryGuard`], preventing the `actors` map from leaking
/// entries when actors die for any reason (Shutdown / panic / mailbox
/// close).
#[derive(Clone)]
pub struct AgentSupervisor {
    actors: Arc<DashMap<SessionId, ActorHandle>>,
    response_tx: mpsc::Sender<AgentOutput>,
}

impl AgentSupervisor {
    pub fn new(response_tx: mpsc::Sender<AgentOutput>) -> Self {
        Self {
            actors: Arc::new(DashMap::new()),
            response_tx,
        }
    }

    /// Send a message to the actor for a given session.
    /// Returns false if the actor doesn't exist.
    pub async fn route(&self, session_id: &SessionId, message: AgentMessage) -> bool {
        // Clone the sender out of the shard lock so we don't hold it
        // across the `.await` below.
        let Some(sender) = self.actors.get(session_id).map(|e| e.sender.clone()) else {
            return false;
        };
        if let Err(e) = sender.send(message).await {
            tracing::warn!(
                %session_id,
                error = %e,
                "failed to send message to actor"
            );
            return false;
        }
        true
    }

    /// Route `message` to the actor for `session_id`, spawning one via
    /// `spawn` if no actor is registered yet. The spawn + insert step is
    /// atomic against the actor map's shard lock, so two concurrent
    /// callers cannot both observe a vacancy and each spawn an actor.
    ///
    /// `spawn` must return a freshly built mailbox sender (typically
    /// produced by an [`crate::actor::router::ActorSpawner`]); the supervisor
    /// inserts it into the registry, then sends `message`. `spawn` runs
    /// only when the entry is vacant, so callers must build the session /
    /// derive the cancellation token before invoking — there's no
    /// "lazy" mode that skips the work when an actor already exists.
    ///
    /// Returns `false` if the eventual `send().await` fails (mailbox
    /// closed); otherwise `true`.
    pub async fn route_or_spawn<F>(
        &self,
        session_id: &SessionId,
        message: AgentMessage,
        spawn: F,
    ) -> bool
    where
        F: FnOnce() -> mpsc::Sender<AgentMessage>,
    {
        use dashmap::Entry;
        let sender = match self.actors.entry(session_id.clone()) {
            Entry::Occupied(slot) => slot.get().sender.clone(),
            Entry::Vacant(slot) => {
                debug!(%session_id, "registering actor");
                let sender = spawn();
                slot.insert(ActorHandle {
                    sender: sender.clone(),
                });
                sender
            }
        };
        if let Err(e) = sender.send(message).await {
            tracing::warn!(
                %session_id,
                error = %e,
                "failed to send message to actor"
            );
            return false;
        }
        true
    }

    /// Atomically insert a new actor handle if no entry exists for
    /// `session_id`. Returns `Ok(())` on insert, or `Err(rejected)`
    /// with the sender the caller tried to register.
    ///
    /// Production code should reach for [`Self::route_or_spawn`] which
    /// folds the build-and-register step into one atomic check; this
    /// raw API is kept for tests and any future caller that genuinely
    /// owns a pre-built mailbox.
    pub fn register_if_absent(
        &self,
        session_id: SessionId,
        sender: mpsc::Sender<AgentMessage>,
    ) -> Result<(), mpsc::Sender<AgentMessage>> {
        use dashmap::Entry;
        match self.actors.entry(session_id.clone()) {
            Entry::Vacant(slot) => {
                debug!(%session_id, "registering actor");
                slot.insert(ActorHandle { sender });
                Ok(())
            }
            Entry::Occupied(_) => Err(sender),
        }
    }

    /// Remove an actor handle. Called by [`ActorRegistryGuard`] when an
    /// actor's `run` loop exits for any reason — Shutdown, mailbox
    /// close, or panic — so the supervisor's map never holds entries
    /// pointing at dead tokio tasks.
    ///
    /// No-op when the entry has already been removed (e.g. an explicit
    /// `shutdown_all` raced ahead of the actor's own guard drop).
    pub fn remove(&self, session_id: &SessionId) {
        if self.actors.remove(session_id).is_some() {
            debug!(%session_id, "deregistering actor");
        }
    }

    /// Returns the number of currently registered actors. Intended for
    /// diagnostics and the idle reaper.
    pub fn len(&self) -> usize {
        self.actors.len()
    }

    /// Returns true when no actors are registered.
    pub fn is_empty(&self) -> bool {
        self.actors.is_empty()
    }

    /// Snapshot of currently registered session ids. The reaper uses
    /// this to consult [`SessionManager`] for last-active timestamps
    /// without holding shard locks across the await boundary.
    pub fn registered_session_ids(&self) -> Vec<SessionId> {
        self.actors.iter().map(|e| e.key().clone()).collect()
    }

    /// Get the response channel sender (for creating new actors).
    pub fn response_tx(&self) -> &mpsc::Sender<AgentOutput> {
        &self.response_tx
    }

    /// Reap actors whose underlying sessions have been idle for longer
    /// than `idle_threshold`.
    ///
    /// Flow per reaped session:
    /// 1. Clear the `session_summaries.in_flight` flag so any maintenance
    ///    child that gets cascade-cancelled below does not leave the
    ///    session permanently blocked from future background compression
    ///    passes. The clear is unconditional (not owner-scoped) because
    ///    the maintenance child is about to die regardless.
    /// 2. Send `AgentMessage::Shutdown` to the actor's mailbox. The
    ///    actor's `run` loop matches `Shutdown`, trips its `actor_token`
    ///    (cascade-killing every maintenance grandchild), and exits.
    ///    The `ActorRegistryGuard` then removes the entry from
    ///    `self.actors` on drop.
    ///
    /// Returns the number of `Shutdown` sends attempted. Idle sessions
    /// that were not registered (cron / maintenance / subagent
    /// one-shots) are skipped — those manage their own lifetime.
    pub async fn reap_idle(&self, sessions: &SessionManager, idle_threshold: Duration) -> usize {
        let candidates = match sessions.idle_sessions(idle_threshold).await {
            Ok(ids) => ids,
            Err(e) => {
                warn!(error = %e, "idle reaper: failed to list idle sessions");
                return 0;
            }
        };
        if candidates.is_empty() {
            return 0;
        }
        let registered: HashSet<SessionId> = self.registered_session_ids().into_iter().collect();
        let mut reaped = 0usize;
        for session_id in candidates {
            if !registered.contains(&session_id) {
                continue;
            }
            if let Err(e) = sessions.clear_summary_in_flight(&session_id).await {
                warn!(
                    %session_id,
                    error = %e,
                    "idle reaper: failed to clear summary in_flight before shutdown"
                );
                // Press on — losing one compression pass slot is
                // better than leaving the actor stuck in memory.
            }
            let Some(sender) = self.actors.get(&session_id).map(|e| e.sender.clone()) else {
                continue;
            };
            match sender.try_send(AgentMessage::Shutdown) {
                Ok(()) => {
                    debug!(%session_id, "idle reaper: sent Shutdown");
                    reaped += 1;
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    debug!(
                        %session_id,
                        "idle reaper: mailbox full, skipping (will retry next tick)"
                    );
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    debug!(
                        %session_id,
                        "idle reaper: mailbox already closed; entry will be removed by registry guard"
                    );
                }
            }
        }
        if reaped > 0 {
            info!(reaped, "idle reaper: shut down idle actors");
        }
        reaped
    }

    /// Shut down all actors gracefully.
    pub async fn shutdown_all(&self) {
        // Snapshot out of the shards before awaiting so each shard
        // lock is released before any `.send().await`.
        let handles: Vec<(SessionId, mpsc::Sender<AgentMessage>)> = self
            .actors
            .iter()
            .map(|e| (e.key().clone(), e.sender.clone()))
            .collect();
        info!(count = handles.len(), "shutting down all actors");
        for (session_id, sender) in handles {
            if let Err(e) = sender.send(AgentMessage::Shutdown).await {
                tracing::warn!(
                    %session_id,
                    error = %e,
                    "failed to send shutdown to actor"
                );
            }
        }
    }
}

/// How often the idle reaper ticks. The reaper itself is cheap (one
/// store query + a few mailbox sends); 5 minutes balances responsiveness
/// against per-tick overhead for many-session deployments.
pub const REAP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// How long a session must have been idle before its actor is eligible
/// for reaping. The session row stays in the store; only the in-memory
/// actor is dropped. The next user message hydrates a fresh actor from
/// the durable state. Session conversation data is never deleted — see
/// CLAUDE.md ("Session data is core data").
pub fn idle_timeout() -> Duration {
    Duration::minutes(30)
}

/// Spawn the idle-actor reaper task.
///
/// Periodically calls [`AgentSupervisor::reap_idle`] every
/// [`REAP_INTERVAL`], shutting down registered actors whose underlying
/// sessions have been idle longer than [`idle_timeout`]. The session
/// row stays in the store; hydration on the next user message rebuilds
/// the actor from durable state.
///
/// The returned [`JoinHandle`] is ignorable; the task exits when
/// `cancel_token` fires (typically tied to the process actor parent).
pub fn spawn_idle_reaper(
    supervisor: AgentSupervisor,
    sessions: Arc<SessionManager>,
    cancel_token: CancellationToken,
) -> JoinHandle<()> {
    let idle_threshold = idle_timeout();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(REAP_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // `interval`'s first tick fires immediately; consume it so the
        // first reap is one full period after boot, not on startup
        // when no actor has had a chance to see a message.
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    debug!("idle reaper: cancelled");
                    break;
                }
                _ = ticker.tick() => {
                    supervisor.reap_idle(&sessions, idle_threshold).await;
                }
            }
        }
    })
}

/// RAII guard that removes an actor entry from [`AgentSupervisor`] when
/// the actor's task exits. Held by the actor's `run` loop so cleanup
/// runs unconditionally — including on panic — and the supervisor's
/// `actors` map cannot accumulate stale handles.
pub struct ActorRegistryGuard {
    supervisor: AgentSupervisor,
    session_id: SessionId,
}

impl ActorRegistryGuard {
    pub fn new(supervisor: AgentSupervisor, session_id: SessionId) -> Self {
        Self {
            supervisor,
            session_id,
        }
    }
}

impl Drop for ActorRegistryGuard {
    fn drop(&mut self) {
        self.supervisor.remove(&self.session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::{ChannelType, SessionId, User};
    use aura_session::test_support::{MemorySessionStore, MemorySessionSummaryStore};
    use aura_session::{SessionStore, SessionSummaryStore};

    fn make_supervisor() -> AgentSupervisor {
        let (tx, _rx) = mpsc::channel(8);
        AgentSupervisor::new(tx)
    }

    #[test]
    fn register_and_remove_round_trip() {
        let supervisor = make_supervisor();
        let session_id = SessionId::from("s-1");
        let (tx, _rx) = mpsc::channel(8);
        supervisor
            .register_if_absent(session_id.clone(), tx)
            .expect("first insert always succeeds");
        assert_eq!(supervisor.len(), 1);
        supervisor.remove(&session_id);
        assert_eq!(supervisor.len(), 0);
    }

    #[test]
    fn registry_guard_removes_on_drop() {
        let supervisor = make_supervisor();
        let session_id = SessionId::from("s-2");
        let (tx, _rx) = mpsc::channel(8);
        supervisor
            .register_if_absent(session_id.clone(), tx)
            .expect("first insert always succeeds");
        assert_eq!(supervisor.len(), 1);
        {
            let _guard = ActorRegistryGuard::new(supervisor.clone(), session_id.clone());
        }
        assert_eq!(supervisor.len(), 0);
    }

    #[test]
    fn double_remove_is_safe() {
        let supervisor = make_supervisor();
        let session_id = SessionId::from("s-3");
        supervisor.remove(&session_id);
        supervisor.remove(&session_id);
    }

    #[test]
    fn register_if_absent_rejects_duplicate() {
        let supervisor = make_supervisor();
        let session_id = SessionId::from("s-dup");
        let (first, _rx) = mpsc::channel(8);
        let (second, _rx2) = mpsc::channel(8);
        supervisor
            .register_if_absent(session_id.clone(), first)
            .expect("first insert");
        let rejected = supervisor
            .register_if_absent(session_id.clone(), second)
            .expect_err("second insert is rejected");
        // The rejected sender is returned to the caller intact —
        // still usable for dispatching `Shutdown` to its actor.
        assert_eq!(rejected.capacity(), 8);
        assert_eq!(supervisor.len(), 1);
    }

    #[tokio::test]
    async fn route_or_spawn_invokes_spawn_when_absent() {
        let supervisor = make_supervisor();
        let session_id = SessionId::from("s-spawn");

        let (tx, mut rx) = mpsc::channel(8);
        let spawn_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let spawn_calls_for_closure = Arc::clone(&spawn_calls);

        let routed = supervisor
            .route_or_spawn(&session_id, AgentMessage::Shutdown, move || {
                spawn_calls_for_closure.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tx
            })
            .await;

        assert!(routed);
        assert_eq!(spawn_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(supervisor.len(), 1, "spawned actor must be registered");
        assert!(matches!(rx.try_recv(), Ok(AgentMessage::Shutdown)));
    }

    #[tokio::test]
    async fn route_or_spawn_reuses_existing_actor() {
        let supervisor = make_supervisor();
        let session_id = SessionId::from("s-reuse");

        let (first, mut first_rx) = mpsc::channel(8);
        supervisor
            .register_if_absent(session_id.clone(), first)
            .expect("seed an existing actor");

        let spawn_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let spawn_calls_for_closure = Arc::clone(&spawn_calls);

        let routed = supervisor
            .route_or_spawn(&session_id, AgentMessage::Shutdown, move || {
                spawn_calls_for_closure.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let (rogue, _) = mpsc::channel(1);
                rogue
            })
            .await;

        assert!(routed);
        assert_eq!(
            spawn_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "spawn closure must not run when an actor is already registered"
        );
        assert_eq!(supervisor.len(), 1);
        // Message reaches the *pre-registered* mailbox, not a freshly
        // built one.
        assert!(matches!(first_rx.try_recv(), Ok(AgentMessage::Shutdown)));
    }

    #[tokio::test]
    async fn route_or_spawn_returns_false_when_send_fails() {
        let supervisor = make_supervisor();
        let session_id = SessionId::from("s-closed");

        let routed = supervisor
            .route_or_spawn(&session_id, AgentMessage::Shutdown, || {
                // Drop the receiver immediately so the freshly built
                // sender is closed before route_or_spawn sends on it.
                let (tx, rx) = mpsc::channel(1);
                drop(rx);
                tx
            })
            .await;

        assert!(!routed, "closed mailbox must surface as routed=false");
        // The actor handle is still inserted — the registry guard will
        // clear it when the (hypothetical) actor task notices its
        // mailbox closed; the supervisor itself does not roll back on
        // send failure.
        assert_eq!(supervisor.len(), 1);
    }

    #[test]
    fn clones_share_state() {
        let supervisor = make_supervisor();
        let clone = supervisor.clone();
        let session_id = SessionId::from("s-4");
        let (tx, _rx) = mpsc::channel(8);
        supervisor
            .register_if_absent(session_id.clone(), tx)
            .expect("first insert always succeeds");
        assert_eq!(clone.len(), 1);
        clone.remove(&session_id);
        assert_eq!(supervisor.len(), 0);
    }

    #[tokio::test]
    async fn reap_idle_shuts_down_only_registered_idle_actors() {
        use chrono::Utc;

        let session_store = Arc::new(MemorySessionStore::new());
        let summary_store = Arc::new(MemorySessionSummaryStore::new());
        let sessions = SessionManager::new(session_store.clone(), summary_store.clone());

        let user = User {
            id: "u-1".to_string(),
            name: None,
            channel: ChannelType::tui(),
        };

        // Idle session — registered with supervisor.
        let mut idle = sessions
            .create_session(user.clone(), ChannelType::tui())
            .await
            .unwrap();
        idle.last_active = Utc::now() - Duration::seconds(120);
        session_store.save(&idle).await.unwrap();

        // Idle session — *not* registered. Reaper must skip.
        let mut idle_unregistered = sessions
            .create_session(user.clone(), ChannelType::tui())
            .await
            .unwrap();
        idle_unregistered.last_active = Utc::now() - Duration::seconds(120);
        session_store.save(&idle_unregistered).await.unwrap();

        // Fresh session — registered but not idle.
        let fresh = sessions
            .create_session(user, ChannelType::tui())
            .await
            .unwrap();

        // Mark idle session as having an in-flight compression so we
        // can verify the reaper clears it.
        sessions
            .mark_summary_in_flight(&idle.id, "owner-x")
            .await
            .unwrap();

        let supervisor = make_supervisor();
        let (idle_tx, mut idle_rx) = mpsc::channel(8);
        let (fresh_tx, mut fresh_rx) = mpsc::channel(8);
        supervisor
            .register_if_absent(idle.id.clone(), idle_tx)
            .expect("first insert");
        supervisor
            .register_if_absent(fresh.id.clone(), fresh_tx)
            .expect("first insert");

        let reaped = supervisor.reap_idle(&sessions, Duration::seconds(60)).await;
        assert_eq!(reaped, 1, "only the registered idle actor is reaped");

        // The idle actor received a Shutdown message.
        assert!(matches!(idle_rx.try_recv(), Ok(AgentMessage::Shutdown)));
        // The fresh actor received nothing.
        assert!(fresh_rx.try_recv().is_err());

        // The in_flight flag on the reaped session is cleared.
        let row = summary_store.get(&idle.id).await.unwrap().unwrap();
        assert!(!row.in_flight);
    }
}
