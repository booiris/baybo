use std::sync::Arc;

use baybo_cron::{CronTriggerEvent, ExecutionCompletion};
use baybo_job::{JobInputKind, JobLifecycle, JobLifecycleEvent, JobPhase, JobStatusKind};
use baybo_model::{
    CronExecution, ExecutionOutcome, PendingCronResult, Session, SessionId, TriggerSource, User,
};
use baybo_session::SessionManager;
use baybo_store::CronStore;
use chrono::Utc;
use chrono_tz::Tz;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::actor::supervisor::AgentSupervisor;
use crate::actor::{AgentMessage, CronDelivery};

use super::{ActorSpawner, Router};

impl Router {
    /// Handle a cron trigger by minting a fresh session and routing a
    /// `CronTrigger` message into a one-shot actor.
    ///
    /// Each fire creates an isolated session so the trigger sees a
    /// clean transcript and a fresh `SessionState` (no leaked
    /// `approved_resources` or compression state from prior fires).
    /// Continuity across fires belongs to memory + skill loading, not to a
    /// shared mutable transcript.
    ///
    /// What the user *sees* — and how the actor is managed — depends on the
    /// schedule:
    ///
    /// - **Recurring** — the fire session is a first-class conversation
    ///   (`conversation: true`), titled after the job, listed in the sidebar,
    ///   and its reply dispatches out through the channel as usual. Each fire
    ///   is its own conversation. Because the user can reply in it, its actor
    ///   is **registered** with the supervisor, so a reply routes to that actor
    ///   rather than forking a second one over the same transcript.
    /// - **One-shot** — the fire session is a private workspace: invisible,
    ///   unopenable, dispatching nothing, and its actor deliberately
    ///   **unregistered** (no follow-up traffic will ever arrive, so a handle
    ///   would just dangle in the supervisor's map). A waiter — spawned here,
    ///   before the trigger is sent, so the terminal event cannot be missed —
    ///   picks the result off the fire job's terminal lifecycle edge and
    ///   delivers it into the conversation that scheduled the job.
    ///
    /// Either way the actor gets `CronTrigger` followed by `ActorStop`; the
    /// priority mailbox serves the trigger first (`ActorStop` is the lowest
    /// tier), so the actor runs the fire and then exits.
    pub(super) async fn handle_cron_trigger(
        &mut self,
        event: CronTriggerEvent,
    ) -> anyhow::Result<()> {
        // A recurring fire's session IS the notification, so it is a listed,
        // titled conversation; a one-shot's is a private workspace whose result
        // is reported elsewhere. Everything below forks on that one fact.
        let conversation = !event.one_shot;
        let session = self.mint_fire_session(&event, conversation).await?;

        debug!(
            session_id = %session.id,
            job_id = %event.job_id,
            one_shot = event.one_shot,
            "routing cron trigger to fresh session"
        );

        let trigger = cron_trigger(&event);
        if conversation {
            self.title_cron_conversation(&session.id, &event).await;
            self.run_conversation_fire(session, trigger).await;
        } else {
            let waiter = self.cron_result_waiter(&event, session.id.clone());
            self.run_oneshot_fire(session, trigger, waiter).await;
        }
        Ok(())
    }

    /// The isolated session this fire runs in.
    async fn mint_fire_session(
        &self,
        event: &CronTriggerEvent,
        conversation: bool,
    ) -> anyhow::Result<Session> {
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
                    origin_session_id: event.origin_session_id.clone(),
                    conversation,
                },
            )
            .await?;
        Ok(session)
    }

    /// The task that will report a one-shot's result to the conversation that
    /// scheduled it.
    ///
    /// Built — and therefore **subscribed** — before the trigger is sent: the
    /// fire's terminal event is published from inside its own turn, so a
    /// subscription opened afterwards could miss it entirely.
    fn cron_result_waiter(
        &self,
        event: &CronTriggerEvent,
        fire_session_id: SessionId,
    ) -> CronResultWaiter {
        CronResultWaiter {
            event: event.clone(),
            fire_session_id,
            lifecycle: Arc::clone(&self.job_lifecycle),
            terminal_rx: self.job_lifecycle.subscribe_lifecycle_events(),
            cron_store: Arc::clone(&self.cron_store),
            delivery: self.cron_result_delivery(),
        }
    }

    /// A **recurring** fire: it owns a conversation the user can reply in, so
    /// its actor is **registered** with the supervisor and **not stopped**.
    ///
    /// Registration is what guarantees the session has exactly one actor: a
    /// reply routes to this one (`route_or_spawn` finds the entry) instead of
    /// forking a second over the same transcript, and the actor deregisters the
    /// id it registered rather than evicting somebody else's handle.
    ///
    /// No `ActorStop` rides behind the trigger, unlike a one-shot's. Stopping
    /// the actor the instant the fire ends is worst exactly when it matters
    /// most — the moment a notification lands is when a user is most likely to
    /// reply — and a reply that raced the stop would be routed into a mailbox
    /// nobody is reading, reported as delivered, and dropped. The actor stays
    /// resident and is reclaimed by the idle reaper, like every conversation's.
    async fn run_conversation_fire(&self, session: Session, trigger: AgentMessage) {
        let session_id = session.id.clone();
        let response_tx = self.supervisor.response_tx().clone();
        let parent_token = self.actor_parent_token.clone();
        let actor_spawner = self.actor_spawner.as_ref();
        let routed = self
            .supervisor
            .route_or_spawn(&session_id, trigger, || {
                let actor_token = parent_token.child_token();
                actor_spawner(session, None, response_tx, actor_token)
            })
            .await;
        if !routed {
            warn!(session_id = %session_id, "failed to deliver cron trigger to its conversation");
        }
    }

    /// A **one-shot** fire: it runs in a private workspace nobody will ever
    /// message again, so its actor is deliberately **unregistered** (a handle in
    /// the supervisor's map would only dangle) and is stopped as soon as the
    /// turn ends. `waiter` reports the result into the conversation that
    /// scheduled the job.
    ///
    /// The waiter is spawned **whatever happens here**, including when the actor
    /// is already gone. The scheduler has recorded this execution as dispatched,
    /// so nothing else will ever retry it: a fire abandoned here would be a
    /// reminder lost in silence. Tripping the actor's token instead makes the
    /// waiter resolve immediately and report the fire as failed.
    async fn run_oneshot_fire(
        &self,
        session: Session,
        trigger: AgentMessage,
        waiter: CronResultWaiter,
    ) {
        let session_id = session.id.clone();
        let response_tx = self.supervisor.response_tx().clone();
        let (mailbox, actor_token) =
            self.spawn_oneshot_actor(session, None, response_tx, &self.actor_parent_token);

        match mailbox.send(trigger).await {
            Ok(()) => {
                // Lowest mailbox priority, so it is served after the fire: the
                // workspace exits as soon as its one turn is done.
                if let Err(e) = mailbox.send(AgentMessage::ActorStop).await {
                    warn!(
                        session_id = %session_id,
                        error = %e,
                        "post-fire shutdown was not queued; the actor still exits when its mailbox drops",
                    );
                }
            }
            Err(e) => {
                warn!(
                    session_id = %session_id,
                    error = %e,
                    "cron fire could not reach its actor; reporting it as a failed fire",
                );
                actor_token.cancel();
            }
        }

        tokio::spawn(waiter.run(actor_token));
    }

    /// Re-deliver one-shot results whose fire completed but whose delivery
    /// never resolved — the process died between the origin's transcript
    /// append and the ledger stamp, or before the append happened at all.
    ///
    /// Idempotent by construction: the origin actor drops an execution it has
    /// already appended (and re-stamps the ledger), so a replay of a delivery
    /// that *did* land costs one no-op message rather than a duplicate row.
    /// Run at router start, before any live traffic.
    pub(super) async fn redrive_cron_deliveries(&self) {
        let awaiting = match self.cron_store.list_executions_awaiting_delivery().await {
            Ok(rows) => rows,
            Err(e) => {
                warn!(error = %e, "failed to scan for undelivered cron results");
                return;
            }
        };
        let pending: Vec<CronExecution> = awaiting
            .into_iter()
            .filter(CronExecution::is_one_shot)
            .collect();
        if pending.is_empty() {
            return;
        }
        info!(
            count = pending.len(),
            "re-driving cron results that never reached their conversation"
        );
        let delivery = self.cron_result_delivery();
        for exec in pending {
            let Some(result) = pending_result_from_execution(&exec) else {
                // No fire session recorded — nothing to read a result out of.
                // Resolve it rather than rescanning this row on every boot.
                warn!(
                    execution_id = %exec.id,
                    "undeliverable cron execution has no fire session; dropping"
                );
                delivery.resolve_ledger(&exec.id).await;
                continue;
            };
            delivery
                .deliver(exec.origin_session_id.clone(), result)
                .await;
        }
    }

    /// The handles a one-shot's result needs to reach its conversation.
    fn cron_result_delivery(&self) -> CronResultDelivery {
        CronResultDelivery {
            session_manager: Arc::clone(&self.session_manager),
            supervisor: self.supervisor.clone(),
            cron_store: Arc::clone(&self.cron_store),
            actor_spawner: Arc::clone(&self.actor_spawner),
            actor_parent_token: self.actor_parent_token.clone(),
        }
    }

    /// Name a recurring fire's conversation `{title} · {M/d}` — the fire date
    /// in the job's own timezone, so a job that fires at 09:00 Shanghai is
    /// dated by its local day rather than by UTC. Deterministic, so no LLM
    /// title pass is needed (and none would run: the titler is gated on a
    /// `User` trigger).
    async fn title_cron_conversation(&self, session_id: &SessionId, event: &CronTriggerEvent) {
        let title = cron_conversation_title(&event.title, &event.timezone);
        if let Err(e) = self
            .session_manager
            .set_title(session_id, Some(&title))
            .await
        {
            warn!(
                session_id = %session_id,
                error = %e,
                "failed to title cron conversation"
            );
        }
    }
}

/// Watches one one-shot fire to its terminal state, records the outcome on the
/// execution, and hands the result to the conversation that scheduled the job.
///
/// Mirrors the subagent waiter (`crate::actor::subagent`): a detached task on
/// the lifecycle bus, with a store reconcile for the cases the bus can't cover
/// (a lagged subscriber, or an actor that died before opening a job at all).
struct CronResultWaiter {
    event: CronTriggerEvent,
    fire_session_id: SessionId,
    lifecycle: Arc<JobLifecycle>,
    terminal_rx: broadcast::Receiver<JobLifecycleEvent>,
    cron_store: Arc<dyn CronStore>,
    delivery: CronResultDelivery,
}

impl CronResultWaiter {
    async fn run(mut self, actor_token: CancellationToken) {
        let outcome = self.await_fire(actor_token).await;
        let completed_at = Utc::now();

        // Record the outcome BEFORE delivering it: a crash in the delivery
        // window then leaves a durable "completed, not yet delivered" row for
        // the boot re-drive, instead of a result nobody remembers.
        if let Err(e) = self
            .cron_store
            .record_execution_completion(
                &self.event.execution_id,
                ExecutionCompletion {
                    fire_session_id: self.fire_session_id.clone(),
                    outcome: outcome.outcome,
                    reply_ordinal: outcome.reply_ordinal,
                    completed_at,
                },
            )
            .await
        {
            warn!(
                execution_id = %self.event.execution_id,
                error = %e,
                "failed to record cron fire completion; delivering anyway"
            );
        }

        let result = PendingCronResult {
            execution_id: self.event.execution_id.clone(),
            cron_job_id: self.event.job_id.clone(),
            job_title: self.event.title.clone(),
            fire_session_id: self.fire_session_id.clone(),
            reply_ordinal: outcome.reply_ordinal,
            outcome: outcome.outcome,
            failure_reason: outcome.failure_reason,
            completed_at,
        };

        self.delivery
            .deliver(self.event.origin_session_id.clone(), result)
            .await;
    }

    /// Wait for the fire's job to reach a terminal state.
    ///
    /// Three exits, all of which notify (silence is the one outcome a
    /// scheduled task must never have):
    /// - the job's terminal lifecycle event — the normal path;
    /// - a lagged bus — reconcile against the store;
    /// - the fire actor stopping without a terminal event (it died before
    ///   opening a job, e.g. the framed prompt could not be appended) — one
    ///   last store reconcile, then report a failure.
    async fn await_fire(&mut self, actor_token: CancellationToken) -> FireOutcome {
        loop {
            tokio::select! {
                event = self.terminal_rx.recv() => match event {
                    Ok(ev) if self.is_our_fire(&ev) => {
                        let Some(kind) = ev.phase.terminal_status() else {
                            continue;
                        };
                        return self.outcome_for(kind, reply_ordinal(&ev.phase)).await;
                    }
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(
                            session_id = %self.fire_session_id,
                            skipped = n,
                            "cron waiter lagged on the lifecycle bus; reconciling via store"
                        );
                        if let Some(outcome) = self.reconcile_via_store().await {
                            return outcome;
                        }
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return FireOutcome::failed("job lifecycle bus closed before the fire finished");
                    }
                },
                _ = actor_token.cancelled() => {
                    // The actor is gone. Either it finished (and we simply
                    // haven't drained its terminal event yet) or it died
                    // before opening a job — the store tells us which.
                    if let Some(outcome) = self.reconcile_via_store().await {
                        return outcome;
                    }
                    return FireOutcome::failed(
                        "the scheduled run stopped before producing a result",
                    );
                }
            }
        }
    }

    fn is_our_fire(&self, ev: &JobLifecycleEvent) -> bool {
        ev.session_id == self.fire_session_id && ev.kind == JobInputKind::Cron
    }

    /// The fire's outcome from the store, or `None` if it has no terminal job
    /// yet (the caller keeps waiting).
    async fn reconcile_via_store(&self) -> Option<FireOutcome> {
        let jobs = match self
            .lifecycle
            .list_by_session(&self.fire_session_id, None)
            .await
        {
            Ok(jobs) => jobs,
            Err(e) => {
                warn!(
                    session_id = %self.fire_session_id,
                    error = %e,
                    "cron waiter store reconcile failed"
                );
                return None;
            }
        };
        let job = jobs
            .into_iter()
            .find(|j| j.input_kind() == JobInputKind::Cron && j.is_terminal())?;
        let ordinal = match &job.final_result {
            Some(baybo_job::JobOutput::Message { ordinal, .. }) => *ordinal,
            _ => None,
        };
        Some(self.outcome_for(job.status.kind(), ordinal).await)
    }

    /// Classify a terminal job state, reading the failure reason off the job
    /// row so the notification can say *why* the scheduled task failed.
    async fn outcome_for(&self, kind: JobStatusKind, reply_ordinal: Option<i64>) -> FireOutcome {
        match kind {
            JobStatusKind::Completed => match reply_ordinal {
                // A completed fire with no persisted reply produced nothing to
                // report — still delivered, as an explicit "no output".
                None => FireOutcome {
                    outcome: ExecutionOutcome::Blank,
                    reply_ordinal: None,
                    failure_reason: None,
                },
                Some(ordinal) => FireOutcome {
                    outcome: ExecutionOutcome::Success,
                    reply_ordinal: Some(ordinal),
                    failure_reason: None,
                },
            },
            _ => FireOutcome {
                outcome: ExecutionOutcome::Failed,
                reply_ordinal: None,
                failure_reason: Some(self.failure_reason().await),
            },
        }
    }

    /// Why the fire failed, in the user's words rather than the job's: the
    /// stored reason when there is one, a generic line otherwise.
    async fn failure_reason(&self) -> String {
        let jobs = self
            .lifecycle
            .list_by_session(&self.fire_session_id, None)
            .await
            .unwrap_or_default();
        jobs.into_iter()
            .filter(|j| j.input_kind() == JobInputKind::Cron)
            .find_map(|j| match j.status {
                baybo_job::JobStatus::Failed { reason } => Some(reason),
                baybo_job::JobStatus::Cancelled { reason, .. } => {
                    Some(format!("cancelled ({reason:?})"))
                }
                _ => None,
            })
            .unwrap_or_else(|| "the scheduled run did not finish".to_string())
    }
}

/// How a fire ended, as the ledger and the notification both need it.
struct FireOutcome {
    outcome: ExecutionOutcome,
    reply_ordinal: Option<i64>,
    failure_reason: Option<String>,
}

impl FireOutcome {
    fn failed(reason: &str) -> Self {
        Self {
            outcome: ExecutionOutcome::Failed,
            reply_ordinal: None,
            failure_reason: Some(reason.to_string()),
        }
    }
}

/// The reply ordinal a terminal phase carries (`Completed` only).
fn reply_ordinal(phase: &JobPhase) -> Option<i64> {
    match phase {
        JobPhase::Completed { reply_ordinal } => *reply_ordinal,
        _ => None,
    }
}

/// Rebuild the delivery payload from a persisted execution — the boot re-drive
/// path. Produces exactly what the waiter would have handed over, so a replayed
/// delivery is identical to the live one.
fn pending_result_from_execution(exec: &CronExecution) -> Option<PendingCronResult> {
    Some(PendingCronResult {
        execution_id: exec.id.clone(),
        cron_job_id: exec.job_id.clone(),
        job_title: exec.display_title(),
        fire_session_id: exec.fire_session_id.clone()?,
        reply_ordinal: exec.reply_ordinal,
        outcome: exec.outcome.unwrap_or(ExecutionOutcome::Blank),
        // The reason was never persisted (only the outcome is); the fire's own
        // session still carries the failed job for anyone who looks.
        failure_reason: None,
        completed_at: exec.completed_at.unwrap_or_else(Utc::now),
    })
}

/// Everything needed to hand a finished one-shot's result to the conversation
/// that scheduled it. Held by the waiter (live path) and rebuilt by the boot
/// re-drive, so both deliver through exactly the same code.
#[derive(Clone)]
pub(super) struct CronResultDelivery {
    session_manager: Arc<SessionManager>,
    supervisor: AgentSupervisor,
    cron_store: Arc<dyn CronStore>,
    actor_spawner: ActorSpawner,
    actor_parent_token: CancellationToken,
}

impl CronResultDelivery {
    /// Deliver `result` into its origin conversation, hydrating that
    /// conversation's actor if the idle reaper already reclaimed it.
    ///
    /// Drops the delivery — resolving the ledger, so it is not retried forever
    /// — when the origin cannot receive it:
    /// - no origin recorded (a job created before origins were stamped);
    /// - the origin session no longer resolves;
    /// - the origin is itself a cron fire session. Chained creation normally
    ///   collapses onto the real conversation at *creation* time (`CronCreate`
    ///   inherits the fire's origin), so this is the backstop for rows created
    ///   before that rule — delivering into an invisible fire session would
    ///   notify nobody.
    async fn deliver(&self, origin_session_id: Option<SessionId>, result: PendingCronResult) {
        let execution_id = result.execution_id.clone();
        let Some(origin) = resolve_origin(&self.session_manager, origin_session_id.as_ref()).await
        else {
            warn!(
                execution_id = %execution_id,
                cron_job_id = %result.cron_job_id,
                origin_session_id = ?origin_session_id,
                "one-shot cron result has no conversation to report to; dropping it"
            );
            self.resolve_ledger(&execution_id).await;
            return;
        };

        let origin_id = origin.id.clone();
        let response_tx = self.supervisor.response_tx().clone();
        let parent_token = self.actor_parent_token.clone();
        let actor_spawner = self.actor_spawner.as_ref();
        let delivered = self
            .supervisor
            .route_or_spawn(
                &origin_id,
                AgentMessage::CronResultReady(Box::new(result)),
                || {
                    let actor_token = parent_token.child_token();
                    let pinned = origin.state.last_llm.clone();
                    actor_spawner(origin, pinned, response_tx, actor_token)
                },
            )
            .await;
        if !delivered {
            // The mailbox is gone (a shutting-down actor). Leave the ledger
            // open: the next boot re-drives it rather than losing the result.
            warn!(
                session_id = %origin_id,
                execution_id = %execution_id,
                "failed to hand cron result to its conversation; leaving it for re-drive"
            );
        }
    }

    /// Stamp an execution's delivery as resolved without notifying anyone —
    /// the result has nowhere to go. Keeps the re-drive scan converging.
    async fn resolve_ledger(&self, execution_id: &str) {
        if let Err(e) = self
            .cron_store
            .mark_execution_notified(execution_id, Utc::now())
            .await
        {
            warn!(
                execution_id = %execution_id,
                error = %e,
                "failed to resolve cron delivery ledger"
            );
        }
    }
}

/// The message that carries a fire to its actor.
fn cron_trigger(event: &CronTriggerEvent) -> AgentMessage {
    AgentMessage::CronTrigger {
        job_id: event.job_id.clone(),
        title: event.title.clone(),
        prompt: event.prompt.clone(),
        delivery: if event.one_shot {
            CronDelivery::OriginSession
        } else {
            CronDelivery::Channel
        },
    }
}

/// The conversation a one-shot's result belongs to, or `None` when it has none
/// that can receive it: no origin recorded, the session is gone, or the origin
/// is a **one-shot fire's own workspace** — invisible and unopenable, so a
/// notification there would reach nobody. The caller then drops the delivery
/// rather than reporting into the void.
///
/// A *recurring* fire's session is a legitimate target: it is a listed,
/// replyable conversation, and a job can genuinely be scheduled from inside one
/// (by the fire, or by a user replying in it). The rule is "can the user open
/// this and read it", not "was it started by cron".
async fn resolve_origin(
    session_manager: &SessionManager,
    origin_session_id: Option<&SessionId>,
) -> Option<Session> {
    let origin_id = origin_session_id?;
    let session = match session_manager.get(origin_id).await {
        Ok(session) => session?,
        Err(e) => {
            warn!(session_id = %origin_id, error = %e, "failed to load cron origin session");
            return None;
        }
    };
    let unreadable_fire_workspace = matches!(session.trigger, TriggerSource::Cron { .. })
        && !session.trigger.is_cron_conversation();
    if unreadable_fire_workspace {
        return None;
    }
    Some(session)
}

/// `{title} · {M/d}` — the fire's date in the job's own timezone. An
/// unparseable zone (a hand-edited row) falls back to UTC rather than dropping
/// the title.
fn cron_conversation_title(title: &str, timezone: &str) -> String {
    let now = Utc::now();
    let local = match timezone.parse::<Tz>() {
        Ok(tz) => now.with_timezone(&tz).format("%-m/%-d").to_string(),
        Err(_) => now.format("%-m/%-d").to_string(),
    };
    format!("{title} · {local}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::mailbox::{self, MailboxReceiver};
    use crate::actor::router::{LiveRateLimit, RouterConfig};
    use crate::security::SecurityGateway;
    use baybo_channels::ChannelRegistry;
    use baybo_cost::test_support::MemoryCostStore;
    use baybo_cost::{CostManager, SpendingLimits};
    use baybo_cron::test_support::InMemoryCronStore;
    use baybo_job::test_support::MemoryJobStore;
    use baybo_model::{ChannelType, CronJob, CronSchedule, CronStatus, User};
    use baybo_security::test_support::MemorySecretStore;
    use baybo_security::{EncryptionKey, LeakDetector, SecretVault};
    use baybo_session::test_support::{
        MemorySessionFolderStore, MemorySessionStore, MemorySessionSummaryStore,
    };
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::mpsc;

    /// Mailboxes the fake spawner handed out, in spawn order.
    type SpawnedActors = Arc<Mutex<Vec<(SessionId, MailboxReceiver<AgentMessage>)>>>;

    /// Router wired to memory stores and a **fake** actor spawner: the spawner
    /// hands back a live mailbox without starting an agent loop, so a test can
    /// drive `handle_cron_trigger` and inspect exactly what the Router did —
    /// which sessions it minted, what it put on the mailbox, and (the point of
    /// these tests) whether it registered the actor.
    struct RouterHarness {
        router: Router,
        sessions: Arc<SessionManager>,
        supervisor: AgentSupervisor,
        cron_store: Arc<InMemoryCronStore>,
        spawned: SpawnedActors,
        /// When set, the fake spawner drops each mailbox receiver immediately,
        /// so the sender it hands back is already closed — the state a
        /// shutdown race leaves behind.
        drop_mailboxes: Arc<AtomicBool>,
        _trigger_tx: mpsc::Sender<CronTriggerEvent>,
        _response_rx: mpsc::Receiver<baybo_channels::AgentOutput>,
    }

    impl RouterHarness {
        fn new() -> Self {
            let (response_tx, response_rx) = mpsc::channel(64);
            let supervisor = AgentSupervisor::new(response_tx);
            let sessions = Arc::new(SessionManager::new(
                Arc::new(MemorySessionStore::new()),
                Arc::new(MemorySessionSummaryStore::new()),
                Arc::new(MemorySessionFolderStore::new()),
            ));

            let spawned: SpawnedActors = Arc::new(Mutex::new(Vec::new()));
            let spawned_for_closure = Arc::clone(&spawned);
            let drop_mailboxes = Arc::new(AtomicBool::new(false));
            let drop_for_closure = Arc::clone(&drop_mailboxes);
            let actor_spawner: ActorSpawner =
                Arc::new(move |session: Session, _llm, _tx, _token| {
                    let (sender, receiver) = mailbox::channel(16);
                    if drop_for_closure.load(Ordering::Relaxed) {
                        drop(receiver);
                    } else {
                        spawned_for_closure.lock().push((session.id, receiver));
                    }
                    sender
                });

            let key = EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap();
            let vault = Arc::new(SecretVault::new(key, Arc::new(MemorySecretStore::new())));
            let security_gateway = Arc::new(SecurityGateway::new(
                Arc::new(LeakDetector::with_default_rules()),
                vault,
            ));

            let cron_store = Arc::new(InMemoryCronStore::new());
            let (trigger_tx, cron_trigger_rx) = mpsc::channel(16);
            let agent_profile_store: Arc<dyn baybo_store::agent_profile::AgentProfileStore> =
                baybo_store::test_support::MemoryAgentProfileStore::new();
            let router = Router::from_config(RouterConfig {
                session_manager: Arc::clone(&sessions),
                supervisor: supervisor.clone(),
                agent_profile_store,
                channels: Arc::new(ChannelRegistry::new()),
                security_gateway,
                cost_manager: CostManager::new(
                    Arc::new(MemoryCostStore::new()),
                    HashMap::new(),
                    SpendingLimits::default(),
                ),
                actor_spawner,
                job_lifecycle: Arc::new(JobLifecycle::new(Arc::new(MemoryJobStore::new()))),
                cron_store: Arc::clone(&cron_store) as Arc<dyn CronStore>,
                cron_trigger_rx,
                actor_parent_token: CancellationToken::new(),
                rate_limit: LiveRateLimit::new(100, std::time::Duration::from_secs(60)),
            });

            Self {
                router,
                sessions,
                supervisor,
                cron_store,
                spawned,
                drop_mailboxes,
                _trigger_tx: trigger_tx,
                _response_rx: response_rx,
            }
        }

        fn event(one_shot: bool) -> CronTriggerEvent {
            CronTriggerEvent {
                job_id: "cj-1".into(),
                execution_id: "ce-1".into(),
                user_id: "u1".into(),
                channel: ChannelType::tui(),
                title: "每日新闻".into(),
                timezone: "UTC".into(),
                prompt: "Summarise the news".into(),
                one_shot,
                origin_session_id: Some(SessionId::from("sess-user")),
            }
        }

        /// Make every subsequently-spawned actor's mailbox already closed.
        fn close_spawned_mailboxes(&self) {
            self.drop_mailboxes.store(true, Ordering::Relaxed);
        }

        /// Record the execution the fire event refers to, as the scheduler
        /// would have before dispatching it — the waiter stamps its outcome
        /// onto this row.
        async fn record_execution(&self) {
            let mut exec = CronExecution::pending(&job("每日新闻"), Utc::now(), Utc::now());
            exec.id = "ce-1".into();
            self.cron_store
                .record_execution(&exec)
                .await
                .expect("record execution");
        }

        /// The session the fake spawner was handed, plus its mailbox.
        fn fire(&self) -> (SessionId, MailboxReceiver<AgentMessage>) {
            self.spawned
                .lock()
                .pop()
                .expect("the router must have spawned an actor for the fire")
        }
    }

    /// A recurring fire's session is a conversation the user can reply in, so
    /// its actor MUST be registered with the supervisor. If it were not, a
    /// reply would find no entry and fork a *second* actor over the same
    /// transcript — and the fire actor's registry guard would then evict the
    /// user's actor on its way out.
    #[tokio::test]
    async fn a_recurring_fires_actor_is_registered_so_a_reply_reaches_it() {
        let mut h = RouterHarness::new();
        h.router
            .handle_cron_trigger(RouterHarness::event(false))
            .await
            .expect("route the fire");

        let (fire_session_id, mut mailbox) = h.fire();
        assert_eq!(
            h.supervisor.registered_session_ids(),
            vec![fire_session_id.clone()],
            "the fire's actor must be in the supervisor's registry"
        );

        assert!(matches!(
            mailbox.try_recv(),
            Ok(AgentMessage::CronTrigger {
                delivery: CronDelivery::Channel,
                ..
            })
        ));
        // And NOT an `ActorStop`: stopping the actor the instant the fire ends
        // is exactly when the user is most likely to reply to the notification
        // they just got, and a reply that raced the stop would be routed into a
        // mailbox nobody is reading and dropped. The idle reaper reclaims this
        // actor like any other conversation's.
        assert!(
            matches!(mailbox.try_recv(), Err(mailbox::TryRecvError::Empty)),
            "a replyable conversation's actor must not be stopped behind its own fire"
        );

        // A user replying in the conversation reaches THAT actor rather than
        // spawning a rival one.
        let before = h.spawned.lock().len();
        assert!(
            h.supervisor
                .route(&fire_session_id, AgentMessage::ActorStop)
                .await,
            "a message for the fire's conversation must route to its live actor"
        );
        assert_eq!(
            h.spawned.lock().len(),
            before,
            "routing to the registered actor must not spawn a second one"
        );

        // The session is a listed, titled conversation.
        let session = h
            .sessions
            .get(&fire_session_id)
            .await
            .unwrap()
            .expect("fire session");
        assert!(session.trigger.is_cron_conversation());
        assert!(
            session
                .title
                .as_deref()
                .is_some_and(|t| t.starts_with("每日新闻 · ")),
            "got {:?}",
            session.title
        );
    }

    /// A one-shot's session is a private workspace with no follow-up traffic:
    /// leaving it unregistered is what keeps the supervisor's map from filling
    /// with dangling handles. It also dispatches nothing — its result is
    /// reported into the conversation that scheduled it.
    #[tokio::test]
    async fn a_one_shot_fires_actor_stays_unregistered_and_silent() {
        let mut h = RouterHarness::new();
        h.router
            .handle_cron_trigger(RouterHarness::event(true))
            .await
            .expect("route the fire");

        let (fire_session_id, mut mailbox) = h.fire();
        assert!(
            h.supervisor.is_empty(),
            "a one-shot's workspace must not be registered"
        );
        assert!(matches!(
            mailbox.try_recv(),
            Ok(AgentMessage::CronTrigger {
                delivery: CronDelivery::OriginSession,
                ..
            })
        ));

        let session = h
            .sessions
            .get(&fire_session_id)
            .await
            .unwrap()
            .expect("fire session");
        assert!(
            !session.trigger.is_cron_conversation(),
            "a one-shot's workspace is not a conversation"
        );
        assert!(session.title.is_none(), "and it is not titled");
        assert_eq!(
            session.trigger.cron_origin_session_id().map(|s| s.as_str()),
            Some("sess-user"),
            "it carries the origin so a chained CronCreate can inherit it",
        );
    }

    /// A one-shot whose actor is already gone when the trigger is handed over
    /// (a shutdown race) must still be reported. The scheduler has recorded the
    /// execution as dispatched, so nothing else will ever retry it: if the
    /// Router bailed here, the user's reminder would vanish in silence. The
    /// waiter runs anyway, on a tripped token, so the fire is stamped failed and
    /// the origin conversation is told.
    #[tokio::test]
    async fn a_one_shot_whose_actor_is_gone_still_reports_a_failure() {
        let mut h = RouterHarness::new();
        h.record_execution().await;
        // The fake spawner hands back a mailbox whose receiver it immediately
        // drops — the closed-mailbox state the race produces.
        h.close_spawned_mailboxes();

        h.router
            .handle_cron_trigger(RouterHarness::event(true))
            .await
            .expect("a dead fire actor is not an error the Router propagates");

        // The waiter resolved the fire rather than leaving it hanging: the
        // execution is stamped, and the ledger records a failure to deliver.
        let execution = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(exec) = h.cron_store.execution("ce-1")
                    && exec.completed_at.is_some()
                {
                    return exec;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the waiter must resolve a fire whose actor never ran");

        assert_eq!(
            execution.outcome,
            Some(ExecutionOutcome::Failed),
            "a fire that never ran is a failed fire, not a silent one",
        );
    }

    fn job(title: &str) -> CronJob {
        CronJob {
            id: "cj-1".into(),
            user_id: "u1".into(),
            channel: ChannelType::tui(),
            title: title.into(),
            schedule: CronSchedule::at(Utc::now()),
            prompt: "do the thing".into(),
            timezone: "UTC".into(),
            status: CronStatus::Enabled,
            last_triggered_at: None,
            next_trigger_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            origin_session_id: None,
        }
    }

    fn sessions() -> Arc<SessionManager> {
        Arc::new(SessionManager::new(
            Arc::new(MemorySessionStore::new()),
            Arc::new(MemorySessionSummaryStore::new()),
            Arc::new(MemorySessionFolderStore::new()),
        ))
    }

    fn user() -> User {
        User {
            id: "u1".into(),
            name: None,
            channel: ChannelType::tui(),
        }
    }

    /// A result reported into a session nobody reads is a lost notification, so
    /// the origin must resolve to a real conversation or the delivery is
    /// dropped outright (and its ledger resolved, so it isn't retried forever).
    #[tokio::test]
    async fn origin_resolves_only_to_a_real_conversation() {
        let sessions = sessions();

        // No origin recorded (a job created before origins were stamped).
        assert!(resolve_origin(&sessions, None).await.is_none());

        // An origin that no longer exists.
        let ghost = SessionId::from("sess-gone");
        assert!(resolve_origin(&sessions, Some(&ghost)).await.is_none());

        // A genuine user conversation — the one deliverable case.
        let convo = sessions
            .create_session(user(), ChannelType::tui())
            .await
            .expect("create user session");
        assert_eq!(
            resolve_origin(&sessions, Some(&convo.id))
                .await
                .map(|s| s.id),
            Some(convo.id.clone()),
        );

        // A recurring fire's session IS a conversation — listed and replyable —
        // so a job scheduled inside one (by the fire, or by a user replying in
        // it) reports back there like anywhere else.
        let recurring = sessions
            .create_session_with_trigger(
                user(),
                ChannelType::tui(),
                TriggerSource::Cron {
                    cron_job_id: "cj-news".into(),
                    origin_session_id: Some(convo.id.clone()),
                    conversation: true,
                },
            )
            .await
            .expect("create recurring fire conversation");
        assert_eq!(
            resolve_origin(&sessions, Some(&recurring.id))
                .await
                .map(|s| s.id),
            Some(recurring.id.clone()),
            "a recurring fire's conversation is a legitimate delivery target",
        );

        // A one-shot fire's workspace is not: it is invisible and unopenable,
        // so a notification there would reach nobody. The delivery is dropped
        // instead of reported into the void.
        let workspace = sessions
            .create_session_with_trigger(
                user(),
                ChannelType::tui(),
                TriggerSource::Cron {
                    cron_job_id: "cj-1".into(),
                    origin_session_id: Some(convo.id.clone()),
                    conversation: false,
                },
            )
            .await
            .expect("create one-shot fire session");
        assert!(
            resolve_origin(&sessions, Some(&workspace.id))
                .await
                .is_none(),
            "a one-shot fire's private workspace must never be a delivery target"
        );
    }

    #[test]
    fn conversation_title_dates_the_fire_in_the_jobs_timezone() {
        // 23:30 UTC on the 1st is already the 2nd in Shanghai — a job that
        // fires "daily at 07:30 Shanghai" must be dated by its own day.
        let title = cron_conversation_title("每日新闻", "Asia/Shanghai");
        assert!(title.starts_with("每日新闻 · "), "{title}");
        // An unparseable zone still yields a titled conversation.
        let fallback = cron_conversation_title("每日新闻", "Mars/Olympus");
        assert!(fallback.starts_with("每日新闻 · "), "{fallback}");
    }

    /// The re-drive rebuilds the delivery payload from the persisted ledger, so
    /// a replayed notification is byte-identical to the one the waiter would
    /// have produced.
    #[test]
    fn redrive_payload_round_trips_from_the_execution_row() {
        let mut exec = CronExecution::pending(&job("晚饭提醒"), Utc::now(), Utc::now());
        exec.fire_session_id = Some("cron-fire".into());
        exec.reply_ordinal = Some(9);
        exec.outcome = Some(ExecutionOutcome::Success);
        exec.completed_at = Some(Utc::now());

        let result = pending_result_from_execution(&exec).expect("has a fire session");
        assert_eq!(result.execution_id, exec.id);
        assert_eq!(result.job_title, "晚饭提醒");
        assert_eq!(result.reply_ordinal, Some(9));
        assert_eq!(result.outcome, ExecutionOutcome::Success);
    }

    /// A fire that never recorded its session has no result to read; the
    /// re-drive must not try to build a delivery out of it.
    #[test]
    fn redrive_payload_absent_without_a_fire_session() {
        let mut exec = CronExecution::pending(&job("x"), Utc::now(), Utc::now());
        exec.completed_at = Some(Utc::now());
        assert!(pending_result_from_execution(&exec).is_none());
    }
}
