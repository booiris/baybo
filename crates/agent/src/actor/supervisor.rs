use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use baybo_channels::{AgentEvent, AgentOutput};
use baybo_model::SessionId;
use chrono::Duration;
use dashmap::DashMap;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::actor::AgentMessage;
use crate::actor::mailbox::{MailboxSender, TrySendError};
use baybo_session::SessionManager;

/// Handle to communicate with a running AgentActor.
pub struct ActorHandle {
    pub sender: MailboxSender<AgentMessage>,
}

/// Per-parent record of one in-flight background job — whatever tool started
/// it — enough for `/stop` to enumerate it (cancel it, report what it was
/// doing) and for the idle reaper to know the parent has outstanding work,
/// without a turn-store lineage walk. Keyed by its handle in the registry.
#[derive(Clone)]
pub struct InFlightJob {
    /// The user-facing handle (`bg-…`) the model, `JobList` / `JobStop` and
    /// the notification share.
    pub handle: baybo_model::BackgroundHandle,
    /// What's running: a subagent's profile name, or `"command"` for a
    /// detached `Bash` command.
    pub kind: String,
    pub task_summary: String,
    /// The session the job itself runs in, when it has one (a subagent's
    /// child), so `/stop` can stamp that session's turn as user-stopped.
    pub session: Option<SessionId>,
    /// The job's cancellation token. `/stop` cancels this directly so a job
    /// dispatched but not yet running is still stopped — a turn-store lookup
    /// would miss it in that window and let it run to completion.
    pub cancel_token: CancellationToken,
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
///
/// `in_flight_background_jobs` tracks background jobs (subagents, detached
/// commands — whatever tool started them) still running per parent session
/// — `parent_session → (handle → [`InFlightJob`])`. The idle reaper consults
/// it before shutting an actor down: a parent with outstanding background
/// work is protected so its mailbox stays alive to receive the eventual
/// `AgentMessage::BackgroundJobFinished` when each job ends. `/stop` drains
/// it to enumerate what to cancel + report, and the removal doubles as the
/// suppress signal (an escort whose entry is gone delivers nothing). Process-local (background dispatches don't survive restart).
#[derive(Clone)]
pub struct AgentSupervisor {
    actors: Arc<DashMap<SessionId, ActorHandle>>,
    response_tx: mpsc::Sender<AgentOutput>,
    in_flight_background_jobs:
        Arc<DashMap<SessionId, HashMap<baybo_model::BackgroundHandle, InFlightJob>>>,
    /// What an idle session's turn is parked on, which decides whether the
    /// reaper may end that turn (see [`Self::reap_idle`]).
    live_asks: Arc<dyn baybo_human_ask::LivePendingAsks>,
    /// The release token of the turn each registered actor is running. The
    /// reaper cancels it to end a turn parked on resumable asks; it is a
    /// child of the actor's own token, so later turns are untouched.
    turn_releases: Arc<DashMap<SessionId, CancellationToken>>,
}

/// Keeps a running turn's release token registered with the supervisor; the
/// registration ends when the turn does.
pub struct TurnReleaseGuard {
    releases: Arc<DashMap<SessionId, CancellationToken>>,
    session_id: SessionId,
}

impl Drop for TurnReleaseGuard {
    fn drop(&mut self) {
        self.releases.remove(&self.session_id);
    }
}

impl AgentSupervisor {
    pub fn new(
        response_tx: mpsc::Sender<AgentOutput>,
        live_asks: Arc<dyn baybo_human_ask::LivePendingAsks>,
    ) -> Self {
        Self {
            actors: Arc::new(DashMap::new()),
            response_tx,
            in_flight_background_jobs: Arc::new(DashMap::new()),
            live_asks,
            turn_releases: Arc::new(DashMap::new()),
        }
    }

    /// End `session_id`'s running turn the way a shutdown would, if every ask
    /// it is parked on is resumable: nothing is appended, each ask stays
    /// pending, and an answer resumes the session through the Resume door.
    /// `false` (and nothing touched) when the turn is not parked, or is
    /// parked on an ask that ending it would abandon.
    pub fn release_parked_turn(&self, session_id: &SessionId) -> bool {
        if self.live_asks.parked_on(session_id) != baybo_human_ask::ParkedOn::Resumable {
            return false;
        }
        let Some(release) = self.turn_releases.get(session_id) else {
            return false;
        };
        info!(%session_id, "ending a turn parked on resumable asks");
        release.cancel();
        true
    }

    /// Register the release token of the turn `session_id`'s actor is about
    /// to run. Drop the guard when the turn ends.
    pub fn track_turn(
        &self,
        session_id: &SessionId,
        release: CancellationToken,
    ) -> TurnReleaseGuard {
        self.turn_releases.insert(session_id.clone(), release);
        TurnReleaseGuard {
            releases: Arc::clone(&self.turn_releases),
            session_id: session_id.clone(),
        }
    }

    /// Mark that `job` now runs in the background for `parent_session_id`.
    /// Pair each call with exactly one [`Self::note_background_job_finished`]
    /// from the escort that sees the job end.
    pub fn note_background_job_started(&self, parent_session_id: &SessionId, job: InFlightJob) {
        self.in_flight_background_jobs
            .entry(parent_session_id.clone())
            .or_default()
            .insert(job.handle.clone(), job);
    }

    /// Counterpart to [`Self::note_background_job_started`]. Removes the
    /// job's record, dropping the parent's entry when its last job clears so
    /// the map doesn't accumulate stale keys. A no-op if `/stop` already
    /// drained it (see [`Self::take_in_flight_background_jobs`]).
    pub fn note_background_job_finished(
        &self,
        parent_session_id: &SessionId,
        handle: &baybo_model::BackgroundHandle,
    ) {
        use dashmap::mapref::entry::Entry;
        // Hold the parent `Entry` guard across the remove + emptiness check so
        // a concurrent `started` can't re-insert into a map we then delete
        // (which would drop the in-flight marker and let the idle reaper reap
        // a parent whose new background job is still running).
        if let Entry::Occupied(mut entry) = self
            .in_flight_background_jobs
            .entry(parent_session_id.clone())
        {
            entry.get_mut().remove(handle);
            if entry.get().is_empty() {
                entry.remove();
            }
        }
    }

    /// Peek whether a specific background job is still tracked. The escort
    /// calls this BEFORE delivering its result: a `false` means `/stop`
    /// already drained the entry, so the delivery must be suppressed (a
    /// user-stopped result must not repopulate the notification buffer).
    pub fn is_background_job_in_flight(
        &self,
        parent_session_id: &SessionId,
        handle: &baybo_model::BackgroundHandle,
    ) -> bool {
        self.in_flight_background_jobs
            .get(parent_session_id)
            .is_some_and(|m| m.contains_key(handle))
    }

    /// True iff at least one background job dispatched from
    /// `parent_session_id` is still running.
    pub fn has_in_flight_background_jobs(&self, parent_session_id: &SessionId) -> bool {
        self.in_flight_background_jobs
            .get(parent_session_id)
            .is_some_and(|m| !m.is_empty())
    }

    /// Remove and return every in-flight background job for
    /// `parent_session_id`. Used by `/stop`: they are the cancellation
    /// targets + ack summaries, and the removal is the suppress signal — each
    /// job's escort will see its entry gone and drop the delivery instead of
    /// re-buffering it.
    pub fn take_in_flight_background_jobs(
        &self,
        parent_session_id: &SessionId,
    ) -> Vec<InFlightJob> {
        self.in_flight_background_jobs
            .remove(parent_session_id)
            .map(|(_, jobs)| jobs.into_values().collect())
            .unwrap_or_default()
    }

    /// Non-draining snapshot of every in-flight background job for a parent —
    /// backs the `JobList` tool.
    pub fn list_in_flight_background(&self, parent_session_id: &SessionId) -> Vec<InFlightJob> {
        self.in_flight_background_jobs
            .get(parent_session_id)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Every in-flight background job across all parents, as
    /// `(parent_session_id, InFlightJob)` — backs the cross-session
    /// `/v1/background-jobs` dashboard. Snapshots into a `Vec` so no shard
    /// lock is held past the call.
    pub fn list_all_in_flight_background(&self) -> Vec<(SessionId, InFlightJob)> {
        self.in_flight_background_jobs
            .iter()
            .flat_map(|parent| {
                let parent_id = parent.key().clone();
                parent
                    .value()
                    .values()
                    .map(move |info| (parent_id.clone(), info.clone()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Cancel and drop ONE in-flight background job by its user-facing
    /// `handle` (`bg-…`) — backs the `JobStop` tool. Cancels the token (which
    /// ends the job) and removes the entry, which doubles as the suppress
    /// signal (the escort finds its entry gone and drops delivery). Returns
    /// the dropped info, or `None` if no such job is in flight for this
    /// parent.
    pub fn cancel_in_flight_background(
        &self,
        parent_session_id: &SessionId,
        handle: &str,
    ) -> Option<InFlightJob> {
        use dashmap::mapref::entry::Entry;
        let mut removed = None;
        if let Entry::Occupied(mut entry) = self
            .in_flight_background_jobs
            .entry(parent_session_id.clone())
        {
            if let Some(info) = entry.get_mut().remove(handle) {
                info.cancel_token.cancel();
                removed = Some(info);
            }
            if entry.get().is_empty() {
                entry.remove();
            }
        }
        removed
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
    /// Deliver to an actor that already exists, or report that none does.
    ///
    /// The spawn half of [`Self::route_or_spawn`] takes a *synchronous*
    /// closure — deliberately, so no `.await` sits inside the `DashMap` entry
    /// lock — which forces every caller to resolve its spawn arguments before
    /// it knows whether a spawn is needed. Callers whose arguments cost
    /// something to resolve (a store read) try this first and pay only on the
    /// miss. Never spawns, so a concurrent reap between this call and the
    /// fallback is just a miss, not a spawn with half-resolved arguments.
    pub async fn route_existing(
        &self,
        session_id: &SessionId,
        message: AgentMessage,
    ) -> Option<bool> {
        let sender = self.actors.get(session_id)?.sender.clone();
        if let Err(e) = sender.send(message).await {
            tracing::warn!(%session_id, error = %e, "failed to send message to actor");
            return Some(false);
        }
        Some(true)
    }

    pub async fn route_or_spawn<F>(
        &self,
        session_id: &SessionId,
        message: AgentMessage,
        spawn: F,
    ) -> bool
    where
        F: FnOnce() -> MailboxSender<AgentMessage>,
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
        sender: MailboxSender<AgentMessage>,
    ) -> Result<(), MailboxSender<AgentMessage>> {
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
    /// Per reaped session: send `AgentMessage::ActorStop` to the actor's
    /// mailbox. The actor's `run` loop matches `ActorStop`, trips its
    /// `actor_token`, and exits; the `ActorRegistryGuard` then removes the
    /// entry from `self.actors` on drop.
    ///
    /// `ActorStop` is only served between turns, so a turn parked on an ask
    /// would hold its actor forever. When every ask it is parked on is
    /// resumable, the reaper also cancels that turn's release token: the turn
    /// ends as it would at shutdown (nothing appended, each ask left pending),
    /// and an answer later resumes the session through the Resume door. A
    /// turn parked on a during-execute approval is left alone — ending it
    /// would abandon the prompt — until the approval's own deadline
    /// (`DURING_EXECUTE_APPROVAL_TIMEOUT`) lets the turn finish.
    ///
    /// Returns the number of `ActorStop` sends attempted. Idle sessions
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
            if self.has_in_flight_background_jobs(&session_id) {
                // Parent has at least one background job still running;
                // reaping now would kill the mailbox before its escort can
                // deliver `AgentMessage::BackgroundJobFinished`. Skip — the
                // next tick after every job finishes will see the counter
                // at zero and proceed.
                debug!(
                    %session_id,
                    "idle reaper: skipping parent with in-flight background subagent(s)"
                );
                continue;
            }
            let parked = self.live_asks.parked_on(&session_id);
            if parked == baybo_human_ask::ParkedOn::NotResumable {
                debug!(
                    %session_id,
                    "idle reaper: skipping a session parked on a during-execute approval"
                );
                continue;
            }
            let Some(sender) = self.actors.get(&session_id).map(|e| e.sender.clone()) else {
                continue;
            };
            match sender.try_send(AgentMessage::ActorStop) {
                Ok(()) => {
                    // After the stop is queued, so the actor is certain to exit
                    // once the parked turn ends.
                    if parked == baybo_human_ask::ParkedOn::Resumable {
                        self.release_parked_turn(&session_id);
                    }
                    debug!(%session_id, "idle reaper: sent Shutdown");
                    reaped += 1;
                }
                Err(TrySendError::Full(_)) => {
                    debug!(
                        %session_id,
                        "idle reaper: mailbox full, skipping (will retry next tick)"
                    );
                }
                Err(TrySendError::Closed(_)) => {
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
        let handles: Vec<(SessionId, MailboxSender<AgentMessage>)> = self
            .actors
            .iter()
            .map(|e| (e.key().clone(), e.sender.clone()))
            .collect();
        info!(count = handles.len(), "shutting down all actors");
        for (session_id, sender) in handles {
            if let Err(e) = sender.send(AgentMessage::ActorStop).await {
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

/// Spawn the per-process turn-state projector — the **sole** producer of
/// the web chat's `Frame::TurnState`.
///
/// Both edges are derived here from the one source of truth (the turn
/// store) rather than emitted by the actor: every turn lifecycle
/// transition publishes on
/// [`baybo_turn::TurnLifecycle::subscribe_lifecycle_events`] (the
/// `Pending → InProgress` start edge and the three terminal edges), and on
/// each one we recompute the session's
/// [`baybo_turn::TurnLifecycle::active_turn_started_at`] and broadcast the
/// current value through the same `response_tx` → channel fan-out the
/// actor's own output uses. The gateway's per-`Subscribe` snapshot reads
/// the *same* `active_turn_started_at` — so the live start broadcast, the
/// live end broadcast, and the join-time snapshot all agree by
/// construction, and the actor never touches `TurnState` at all.
///
/// Recompute-is-truth keeps this robust to *which* turn's transition fired
/// it: the turn itself or a child-session subagent just means "recompute this
/// session now", and the broadcast is whatever is currently true — never a
/// stale "X started/ended". A `Lagged` drop is harmless: the next event
/// recomputes, and the Subscribe snapshot covers a client that joined during
/// the gap.
pub fn spawn_turn_state_projector(
    turn_lifecycle: Arc<baybo_turn::TurnLifecycle>,
    sessions: Arc<SessionManager>,
    response_tx: mpsc::Sender<AgentOutput>,
    cancel_token: CancellationToken,
) -> JoinHandle<()> {
    // Subscribe synchronously, before the spawn returns, so no lifecycle
    // event published after this call can slip through unobserved.
    let mut events = turn_lifecycle.subscribe_lifecycle_events();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => break,
                recv = events.recv() => match recv {
                    Ok(ev) => {
                        project_turn_state(&turn_lifecycle, &sessions, &response_tx, &ev.session_id)
                            .await
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        debug!(skipped = n, "turn-state projector lagged; next event recomputes");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
        debug!("turn-state projector: stopped");
    })
}

/// Recompute one session's turn activity from the turn store and broadcast
/// the current `TurnState`. The Subscribe-time snapshot
/// (`gateway::channel::route`) reads the same `active_turn_started_at`
/// directly, so a freshly-joined tab and a live broadcast never disagree.
async fn project_turn_state(
    turn_lifecycle: &baybo_turn::TurnLifecycle,
    sessions: &SessionManager,
    response_tx: &mpsc::Sender<AgentOutput>,
    session_id: &SessionId,
) {
    let started_at = match turn_lifecycle.active_turn_started_at(session_id).await {
        Ok(started_at) => started_at,
        Err(e) => {
            warn!(%session_id, error = %e, "turn-state projector: recompute failed");
            return;
        }
    };
    // TurnState is consumed only by the web dashboard; a session-row miss
    // falls back to the reserved http channel so the frame still reaches
    // web subscribers (every other surface drops it regardless).
    let (channel, user_id) = match sessions.get(session_id).await {
        Ok(Some(s)) => (s.channel, s.user.id),
        _ => (baybo_model::ChannelType::owner(), String::new()),
    };
    let out = AgentOutput {
        session_id: session_id.clone(),
        user_id,
        channel,
        event: AgentEvent::TurnState {
            active: started_at.is_some(),
            started_at,
        },
    };
    let _ = response_tx.send(out).await;
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
    use crate::actor::mailbox;
    use baybo_model::{ChannelType, SessionId, User};
    use baybo_session::SessionStore;
    use baybo_session::test_support::{MemorySessionFolderStore, MemorySessionStore};

    fn make_supervisor() -> AgentSupervisor {
        let (tx, _rx) = mpsc::channel::<AgentOutput>(8);
        AgentSupervisor::new(
            tx,
            std::sync::Arc::new(baybo_human_ask::test_support::NoLivePendingAsks),
        )
    }

    #[test]
    fn register_and_remove_round_trip() {
        let supervisor = make_supervisor();
        let session_id = SessionId::from("s-1");
        let (tx, _rx) = mailbox::channel(8);
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
        let (tx, _rx) = mailbox::channel(8);
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
        let (first, _rx) = mailbox::channel(8);
        let (second, _rx2) = mailbox::channel(8);
        supervisor
            .register_if_absent(session_id.clone(), first)
            .expect("first insert");
        let rejected = supervisor
            .register_if_absent(session_id.clone(), second)
            .expect_err("second insert is rejected");
        // The rejected sender is returned to the caller intact —
        // still usable for dispatching `ActorStop` to its actor.
        assert!(rejected.try_send(AgentMessage::ActorStop).is_ok());
        assert_eq!(supervisor.len(), 1);
    }

    #[tokio::test]
    async fn route_or_spawn_invokes_spawn_when_absent() {
        let supervisor = make_supervisor();
        let session_id = SessionId::from("s-spawn");

        let (tx, mut rx) = mailbox::channel(8);
        let spawn_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let spawn_calls_for_closure = Arc::clone(&spawn_calls);

        let routed = supervisor
            .route_or_spawn(&session_id, AgentMessage::ActorStop, move || {
                spawn_calls_for_closure.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tx
            })
            .await;

        assert!(routed);
        assert_eq!(spawn_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(supervisor.len(), 1, "spawned actor must be registered");
        assert!(matches!(rx.try_recv(), Ok(AgentMessage::ActorStop)));
    }

    #[tokio::test]
    async fn route_or_spawn_reuses_existing_actor() {
        let supervisor = make_supervisor();
        let session_id = SessionId::from("s-reuse");

        let (first, mut first_rx) = mailbox::channel(8);
        supervisor
            .register_if_absent(session_id.clone(), first)
            .expect("seed an existing actor");

        let spawn_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let spawn_calls_for_closure = Arc::clone(&spawn_calls);

        let routed = supervisor
            .route_or_spawn(&session_id, AgentMessage::ActorStop, move || {
                spawn_calls_for_closure.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let (rogue, _) = mailbox::channel(1);
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
        assert!(matches!(first_rx.try_recv(), Ok(AgentMessage::ActorStop)));
    }

    #[tokio::test]
    async fn route_or_spawn_returns_false_when_send_fails() {
        let supervisor = make_supervisor();
        let session_id = SessionId::from("s-closed");

        let routed = supervisor
            .route_or_spawn(&session_id, AgentMessage::ActorStop, || {
                // Drop the receiver immediately so the freshly built
                // sender is closed before route_or_spawn sends on it.
                let (tx, rx) = mailbox::channel(1);
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
        let (tx, _rx) = mailbox::channel(8);
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
        let folder_store = Arc::new(MemorySessionFolderStore::new());
        let sessions = SessionManager::new(session_store.clone(), folder_store);

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

        let supervisor = make_supervisor();
        let (idle_tx, mut idle_rx) = mailbox::channel(8);
        let (fresh_tx, mut fresh_rx) = mailbox::channel(8);
        supervisor
            .register_if_absent(idle.id.clone(), idle_tx)
            .expect("first insert");
        supervisor
            .register_if_absent(fresh.id.clone(), fresh_tx)
            .expect("first insert");

        let reaped = supervisor.reap_idle(&sessions, Duration::seconds(60)).await;
        assert_eq!(reaped, 1, "only the registered idle actor is reaped");

        // The idle actor received a Shutdown message.
        assert!(matches!(idle_rx.try_recv(), Ok(AgentMessage::ActorStop)));
        // The fresh actor received nothing.
        assert!(fresh_rx.try_recv().is_err());
    }

    /// A probe that reports what each listed session is parked on.
    struct Parked(Vec<(SessionId, baybo_human_ask::ParkedOn)>);

    impl baybo_human_ask::LivePendingAsks for Parked {
        fn parked_on(&self, session_id: &SessionId) -> baybo_human_ask::ParkedOn {
            self.0
                .iter()
                .find(|(id, _)| id == session_id)
                .map_or(baybo_human_ask::ParkedOn::Nothing, |(_, parked)| *parked)
        }
    }

    #[tokio::test]
    async fn reap_idle_ends_a_resumable_park_and_keeps_a_during_execute_one() {
        use baybo_human_ask::ParkedOn;
        use chrono::Utc;

        let session_store = Arc::new(MemorySessionStore::new());
        let folder_store = Arc::new(MemorySessionFolderStore::new());
        let sessions = SessionManager::new(session_store.clone(), folder_store);
        let user = User {
            id: "u-1".to_string(),
            name: None,
            channel: ChannelType::tui(),
        };
        let idle_session = || {
            let sessions = &sessions;
            let session_store = &session_store;
            let user = user.clone();
            async move {
                let mut session = sessions
                    .create_session(user, ChannelType::tui())
                    .await
                    .unwrap();
                session.last_active = Utc::now() - Duration::seconds(120);
                session_store.save(&session).await.unwrap();
                session.id
            }
        };
        let resumable = idle_session().await;
        let during = idle_session().await;

        let (tx, _rx) = mpsc::channel::<AgentOutput>(8);
        let supervisor = AgentSupervisor::new(
            tx,
            Arc::new(Parked(vec![
                (resumable.clone(), ParkedOn::Resumable),
                (during.clone(), ParkedOn::NotResumable),
            ])),
        );
        let (resumable_tx, mut resumable_rx) = mailbox::channel(8);
        let (during_tx, mut during_rx) = mailbox::channel(8);
        supervisor
            .register_if_absent(resumable.clone(), resumable_tx)
            .expect("first insert");
        supervisor
            .register_if_absent(during.clone(), during_tx)
            .expect("first insert");
        let resumable_release = CancellationToken::new();
        let during_release = CancellationToken::new();
        let _resumable_turn = supervisor.track_turn(&resumable, resumable_release.clone());
        let _during_turn = supervisor.track_turn(&during, during_release.clone());

        let reaped = supervisor.reap_idle(&sessions, Duration::seconds(60)).await;
        assert_eq!(reaped, 1);
        assert!(
            resumable_release.is_cancelled(),
            "the turn parked on resumable asks is ended"
        );
        assert!(matches!(
            resumable_rx.try_recv(),
            Ok(AgentMessage::ActorStop)
        ));
        assert!(
            !during_release.is_cancelled(),
            "a during-execute approval keeps its turn"
        );
        assert!(during_rx.try_recv().is_err(), "and its actor stays warm");
    }

    #[test]
    fn a_turn_release_is_tracked_only_while_its_guard_lives() {
        let sup = make_supervisor();
        let id = SessionId::from("s");
        let guard = sup.track_turn(&id, CancellationToken::new());
        assert!(sup.turn_releases.contains_key(&id));
        drop(guard);
        assert!(!sup.turn_releases.contains_key(&id));
    }

    #[test]
    fn background_subagent_tracking_adds_and_clears() {
        let sup = make_supervisor();
        let id = SessionId::from("parent");
        let c1 = SessionId::from("child-1");
        let c2 = SessionId::from("child-2");
        assert!(!sup.has_in_flight_background_jobs(&id));
        sup.note_background_job_started(
            &id,
            InFlightJob {
                handle: "bg-c1".into(),
                kind: "explorer".to_string(),
                task_summary: "find X".to_string(),
                session: Some(c1.clone()),
                cancel_token: CancellationToken::new(),
            },
        );
        sup.note_background_job_started(
            &id,
            InFlightJob {
                handle: "bg-c2".into(),
                kind: "planner".to_string(),
                task_summary: "draft Y".to_string(),
                session: Some(c2.clone()),
                cancel_token: CancellationToken::new(),
            },
        );
        assert!(sup.has_in_flight_background_jobs(&id));
        assert!(sup.is_background_job_in_flight(&id, &"bg-c1".into()));
        sup.note_background_job_finished(&id, &"bg-c1".into());
        assert!(
            sup.has_in_flight_background_jobs(&id),
            "still one in-flight after one finished"
        );
        assert!(!sup.is_background_job_in_flight(&id, &"bg-c1".into()));
        sup.note_background_job_finished(&id, &"bg-c2".into());
        assert!(
            !sup.has_in_flight_background_jobs(&id),
            "map must clear once every dispatch finishes"
        );
    }

    #[test]
    fn take_in_flight_drains_and_suppresses() {
        // `/stop` drains the registry; afterwards each child reads as
        // not-in-flight, so its escort suppresses the delivery. The
        // drained entries carry the child's cancel token so `/stop` can stop a
        // subagent that hasn't created its turn row yet (the pre-turn window).
        let sup = make_supervisor();
        let id = SessionId::from("parent");
        let c1 = SessionId::from("child-1");
        let c2 = SessionId::from("child-2");
        let tok1 = CancellationToken::new();
        sup.note_background_job_started(
            &id,
            InFlightJob {
                handle: "bg-c1".into(),
                kind: "explorer".to_string(),
                task_summary: "find X".to_string(),
                session: Some(c1.clone()),
                cancel_token: tok1.clone(),
            },
        );
        sup.note_background_job_started(
            &id,
            InFlightJob {
                handle: "bg-c2".into(),
                kind: "planner".to_string(),
                task_summary: "draft Y".to_string(),
                session: Some(c2.clone()),
                cancel_token: CancellationToken::new(),
            },
        );
        let mut taken = sup.take_in_flight_background_jobs(&id);
        taken.sort_by(|a, b| a.task_summary.cmp(&b.task_summary));
        assert_eq!(taken.len(), 2);
        assert_eq!(taken[0].task_summary, "draft Y");
        assert_eq!(taken[1].task_summary, "find X");
        // The drained entry's token is the live child handle: cancelling it
        // (what `/stop` does) trips the original token.
        assert!(!tok1.is_cancelled());
        taken[1].cancel_token.cancel();
        assert!(
            tok1.is_cancelled(),
            "/stop stops the child via its stored token"
        );
        assert!(!sup.has_in_flight_background_jobs(&id));
        assert!(!sup.is_background_job_in_flight(&id, &"bg-c1".into()));
        // A second drain is empty (idempotent), and `finished` is a no-op.
        assert!(sup.take_in_flight_background_jobs(&id).is_empty());
        sup.note_background_job_finished(&id, &"bg-c1".into());
    }

    #[test]
    fn background_subagent_finished_on_unknown_parent_is_a_noop() {
        let sup = make_supervisor();
        // Removing without a prior start must not panic — the escort's
        // `finished` is unconditional, so this edge case is reachable if the
        // spawn path bailed before registering (or `/stop` already drained).
        sup.note_background_job_finished(&SessionId::from("ghost"), &"bg-ghost".into());
        assert!(!sup.has_in_flight_background_jobs(&SessionId::from("ghost")));
    }

    #[test]
    fn cancel_in_flight_background_matches_by_advertised_handle() {
        let sup = make_supervisor();
        let parent = SessionId::from("parent");
        // The handle the agent sees (and passes to JobStop) is the registry
        // key, distinct from the session the job runs in.
        let child = SessionId::from("child-xyz");
        let tok = CancellationToken::new();
        sup.note_background_job_started(
            &parent,
            InFlightJob {
                handle: "bg-handle-1".into(),
                kind: "explorer".to_string(),
                task_summary: "find X".to_string(),
                session: Some(child.clone()),
                cancel_token: tok.clone(),
            },
        );
        // JobStop by the advertised handle must find it.
        let removed = sup.cancel_in_flight_background(&parent, "bg-handle-1");
        assert!(removed.is_some(), "must match by advertised handle");
        assert!(tok.is_cancelled(), "the child's token is cancelled");
        assert!(!sup.has_in_flight_background_jobs(&parent));
        // An unknown handle is a no-op.
        assert!(
            sup.cancel_in_flight_background(&parent, "bg-nope")
                .is_none()
        );
    }

    #[test]
    fn list_all_in_flight_background_spans_parents() {
        let sup = make_supervisor();
        let p1 = SessionId::from("p1");
        let p2 = SessionId::from("p2");
        sup.note_background_job_started(
            &p1,
            InFlightJob {
                handle: "bg-1".into(),
                kind: "explorer".to_string(),
                task_summary: "a".to_string(),
                session: Some(SessionId::from("c1").clone()),
                cancel_token: CancellationToken::new(),
            },
        );
        sup.note_background_job_started(
            &p1,
            InFlightJob {
                handle: "bg-2".into(),
                kind: "planner".to_string(),
                task_summary: "b".to_string(),
                session: Some(SessionId::from("c2").clone()),
                cancel_token: CancellationToken::new(),
            },
        );
        sup.note_background_job_started(
            &p2,
            InFlightJob {
                handle: "bg-3".into(),
                kind: "reviewer".to_string(),
                task_summary: "c".to_string(),
                session: Some(SessionId::from("c3").clone()),
                cancel_token: CancellationToken::new(),
            },
        );
        let all = sup.list_all_in_flight_background();
        assert_eq!(all.len(), 3, "collects across both parents");
        let mut handles: Vec<_> = all.iter().map(|(_, info)| info.handle.clone()).collect();
        handles.sort();
        assert_eq!(handles, vec!["bg-1", "bg-2", "bg-3"]);
        // Both parent ids are represented.
        assert!(all.iter().any(|(p, _)| p == &p1));
        assert!(all.iter().any(|(p, _)| p == &p2));
    }

    #[tokio::test]
    async fn reap_idle_skips_parent_with_in_flight_background_subagent() {
        use chrono::Utc;

        let session_store = Arc::new(MemorySessionStore::new());
        let folder_store = Arc::new(MemorySessionFolderStore::new());
        let sessions = SessionManager::new(session_store.clone(), folder_store);

        let user = User {
            id: "u".to_string(),
            name: None,
            channel: ChannelType::tui(),
        };
        let mut parent = sessions
            .create_session(user, ChannelType::tui())
            .await
            .unwrap();
        parent.last_active = Utc::now() - Duration::seconds(120);
        session_store.save(&parent).await.unwrap();

        let supervisor = make_supervisor();
        let (mailbox_tx, mut mailbox_rx) = mailbox::channel(8);
        supervisor
            .register_if_absent(parent.id.clone(), mailbox_tx)
            .expect("first insert");

        // Dispatch a background subagent.
        let child = SessionId::from("child");
        supervisor.note_background_job_started(
            &parent.id,
            InFlightJob {
                handle: "bg-child".into(),
                kind: "explorer".to_string(),
                task_summary: "task".to_string(),
                session: Some(child.clone()),
                cancel_token: CancellationToken::new(),
            },
        );

        // Reaper should skip the parent.
        let reaped = supervisor.reap_idle(&sessions, Duration::seconds(60)).await;
        assert_eq!(reaped, 0, "parent must be protected while subagent runs");
        assert!(mailbox_rx.try_recv().is_err(), "no Shutdown queued");

        // Once the subagent finishes, the reaper proceeds on the next tick.
        supervisor.note_background_job_finished(&parent.id, &"bg-child".into());
        let reaped = supervisor.reap_idle(&sessions, Duration::seconds(60)).await;
        assert_eq!(reaped, 1, "parent must be reaped after subagent clears");
        assert!(matches!(mailbox_rx.try_recv(), Ok(AgentMessage::ActorStop)));
    }
}
