//! `JobLifecycle` — thin orchestration layer over `JobStore` for the
//! Job state machine. See `docs/modules/job.md` for the design.

use std::sync::Arc;

#[cfg(test)]
use crate::JobStatus;
use crate::cancellation_registry::{JobCancellationGuard, JobCancellationRegistry};
use crate::{CancelReason, Job, JobError, JobInput, JobShape, JobStatusKind, JobTransition};
use aura_model::{JobId, SessionId, SpanId, TriggerKind};
use aura_store::JobStore;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

type Result<T> = std::result::Result<T, JobError>;

/// Process-wide capacity for the lifecycle-event broadcast bus.
///
/// Sized to absorb the burst a multi-job subagent fan-out can produce
/// without lagging the parent's wait loop. Subscribers that lag still
/// reconcile against the store (`list_by_session` /
/// `active_turn_started_at`) so dropped events don't cause lost
/// terminations or stale turn state — capacity is a latency knob, not
/// correctness.
const JOB_LIFECYCLE_EVENT_CAPACITY: usize = 256;

/// Which lifecycle transition a [`JobLifecycleEvent`] marks. The bus
/// publishes the `Pending → InProgress` **start** edge and the three
/// terminal edges; the intermediate `stuck` / `recover` transitions are
/// not published (rare maintenance moves; the store stays the source of
/// truth for them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobPhase {
    Started,
    Completed,
    Failed,
    Cancelled,
}

impl JobPhase {
    /// The terminal job status this phase denotes, or `None` for
    /// [`JobPhase::Started`] — lets a terminal-only consumer (the
    /// subagent waiter) filter out the start edge in one match.
    pub fn terminal_status(self) -> Option<JobStatusKind> {
        match self {
            JobPhase::Started => None,
            JobPhase::Completed => Some(JobStatusKind::Completed),
            JobPhase::Failed => Some(JobStatusKind::Failed),
            JobPhase::Cancelled => Some(JobStatusKind::Cancelled),
        }
    }
}

/// A job lifecycle transition published by `JobLifecycle` on the
/// broadcast bus: the `Pending → InProgress` start edge and the three
/// terminal edges. Carries the minimum identifiers a subscriber needs to
/// filter without going back to the store: the `JobId`, its session, and
/// the optional parent for hierarchy-scoped waits (subagent path).
///
/// Two consumers today: the subagent runtime waits for a child's terminal
/// edge (`phase.terminal_status()`), and the turn-state projector
/// recomputes a session's turn activity on any edge (it ignores `phase` —
/// every edge just means "recompute this session now").
#[derive(Debug, Clone)]
pub struct JobLifecycleEvent {
    pub job_id: JobId,
    pub session_id: SessionId,
    pub parent_job_id: Option<JobId>,
    pub phase: JobPhase,
}

/// Owns the job state machine + persistence orchestration. Pure
/// passthrough to `Job`'s internal transition validation —
/// `JobLifecycle` itself contains no state machine logic.
pub struct JobLifecycle {
    store: Arc<dyn JobStore>,
    /// Process-wide registry of `JobId → CancellationToken` for
    /// in-flight jobs. `cancel()` trips the matching token (if any)
    /// in addition to flipping the DB row.
    cancellation: Arc<JobCancellationRegistry>,
    /// Fire-and-forget bus for lifecycle transitions (start + terminal).
    /// The subagent runtime subscribes to unblock the parent on a child's
    /// terminal edge; the turn-state projector subscribes to drive the
    /// web chat's per-session turn activity off both edges.
    lifecycle_events: broadcast::Sender<JobLifecycleEvent>,
}

impl JobLifecycle {
    pub fn new(store: Arc<dyn JobStore>) -> Self {
        let (lifecycle_events, _rx) = broadcast::channel(JOB_LIFECYCLE_EVENT_CAPACITY);
        Self {
            store,
            cancellation: Arc::new(JobCancellationRegistry::new()),
            lifecycle_events,
        }
    }

    /// Subscribe to lifecycle events (start + terminal). Subscribers must
    /// reconcile against the store on
    /// `broadcast::error::RecvError::Lagged` — a dropped event is not
    /// re-published (the subagent waiter re-checks via the store; the
    /// turn-state projector recomputes on the next event, with the
    /// Subscribe snapshot covering the gap).
    pub fn subscribe_lifecycle_events(&self) -> broadcast::Receiver<JobLifecycleEvent> {
        self.lifecycle_events.subscribe()
    }

    /// Register an in-flight job's cancellation token. The returned
    /// guard unregisters on drop, so the agent loop's early-`?` paths
    /// can't leak entries. `cancel(job_id, ...)` while the guard is
    /// alive will trip the token (and only afterwards flip the DB
    /// row), so the in-flight execution sees the cancel before
    /// terminal-state observers do.
    pub fn register_running(
        &self,
        job_id: JobId,
        token: CancellationToken,
    ) -> JobCancellationGuard {
        self.cancellation.register(job_id, token);
        JobCancellationGuard::new(Arc::clone(&self.cancellation), job_id)
    }

    /// Create a new job in `Pending`.
    ///
    /// `origin` is the root trigger kind of the owning session — stored
    /// on the job as-is (see `aura_job::kind`), independent of `input`.
    /// `shape` is declared by the caller (the running code path), not
    /// inferred from `input`.
    ///
    /// **Production code does not call this directly** — it goes
    /// through `agent::scope::with_job` (which builds a `JobSpec`
    /// and owns the full create→start→run→complete chain) so the
    /// cancel state machine can't be skipped. The method is left
    /// `pub` only for tests in this crate (and downstream test
    /// fixtures) that need to construct jobs in arbitrary states.
    pub async fn start_job(
        &self,
        session_id: SessionId,
        origin: TriggerKind,
        shape: JobShape,
        input: JobInput,
        parent_job_id: Option<JobId>,
    ) -> Result<Job> {
        let job = Job::new(session_id, origin, shape, input, parent_job_id);
        self.store.create(&job.to_row()?).await?;
        Ok(job)
    }

    /// Move `Pending → InProgress`. Publishes a [`JobPhase::Started`]
    /// event — the start edge the turn-state projector turns into the
    /// web chat's "turn began".
    pub async fn start(&self, job_id: &JobId) -> Result<()> {
        let (job, transition) = self.apply(job_id, |j| j.start()).await?;
        self.persist_and_publish(job, transition, JobPhase::Started)
            .await
    }

    /// Move `InProgress → Completed` with a final output.
    pub async fn complete(&self, job_id: &JobId, output: crate::JobOutput) -> Result<()> {
        let (job, transition) = self.apply(job_id, |j| j.complete(output)).await?;
        self.persist_and_publish(job, transition, JobPhase::Completed)
            .await
    }

    /// Move to `Failed { reason }`.
    pub async fn fail(&self, job_id: &JobId, reason: impl Into<String>) -> Result<()> {
        let reason = reason.into();
        let (job, transition) = self.apply(job_id, |j| j.fail(&reason)).await?;
        self.persist_and_publish(job, transition, JobPhase::Failed)
            .await
    }

    /// Move to `Cancelled { reason, partial_artifacts }`.
    ///
    /// Trips the in-flight cancellation token (if any) before flipping
    /// the DB row, so the running execution observes the cancel and
    /// can short-circuit instead of running to completion.
    ///
    /// Idempotent on terminal jobs: a cancel that races against natural
    /// completion returns `Ok(())` without touching the row, so admin
    /// callers don't see a 500 for a job that finished a moment earlier.
    pub async fn cancel(
        &self,
        job_id: &JobId,
        reason: CancelReason,
        partial_artifacts: Vec<SpanId>,
    ) -> Result<()> {
        self.cancellation.cancel(job_id);
        let job = self.load_job(job_id).await?;
        if job.is_terminal() {
            return Ok(());
        }
        let (job, transition) = self
            .apply(job_id, |j| j.cancel(reason, partial_artifacts.clone()))
            .await?;
        self.persist_and_publish(job, transition, JobPhase::Cancelled)
            .await
    }

    /// Boot-time recovery cancel. Same state-machine semantics as
    /// [`Self::cancel`], but `ended_at` is set to the supplied `at`
    /// (typically `max(child_step.ended_at)`) instead of `Utc::now()` —
    /// the process may have crashed long before the next start, so using
    /// the wall-clock at recovery time would distort duration metrics.
    ///
    /// No registered token to trip (the job is by definition no longer
    /// running), so this skips the `cancellation.cancel` call.
    pub async fn cancel_at(
        &self,
        job_id: &JobId,
        reason: CancelReason,
        partial_artifacts: Vec<SpanId>,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        let job = self.load_job(job_id).await?;
        if job.is_terminal() {
            return Ok(());
        }
        let (job, transition) = self
            .apply(job_id, |j| {
                j.cancel_at(reason, partial_artifacts.clone(), at)
            })
            .await?;
        self.persist_and_publish(job, transition, JobPhase::Cancelled)
            .await
    }

    /// Move to `Stuck { reason }` from `InProgress`.
    pub async fn stuck(&self, job_id: &JobId, reason: impl Into<String>) -> Result<()> {
        let reason = reason.into();
        let (job, transition) = self.apply(job_id, |j| j.stuck(&reason)).await?;
        self.persist(job, transition).await
    }

    /// Move `Stuck → InProgress`.
    pub async fn recover(&self, job_id: &JobId, reason: impl Into<String>) -> Result<()> {
        let reason = reason.into();
        let (job, transition) = self.apply(job_id, |j| j.recover(&reason)).await?;
        self.persist(job, transition).await
    }

    pub async fn get(&self, job_id: &JobId) -> Result<Option<Job>> {
        self.store.get(job_id).await?.map(Job::from_row).transpose()
    }

    /// List jobs, optionally filtered by status discriminator. Newest
    /// `created_at` first.
    pub async fn list(&self, status: Option<JobStatusKind>) -> Result<Vec<Job>> {
        let mut rows = match status {
            Some(k) => self.store.list_by_status_kind(k.as_snake_case()).await?,
            None => self.store.list_all().await?,
        };
        rows.sort_by_key(|b| std::cmp::Reverse(b.created_at));
        rows.into_iter().map(Job::from_row).collect()
    }

    /// List every non-terminal job (`Pending` / `InProgress` / `Stuck`)
    /// in one query. One trip to the store instead of three —
    /// `QueryApi::find_recoverable_jobs` used to call `list(Some(k))`
    /// for each kind serially.
    pub async fn list_recoverable(&self) -> Result<Vec<Job>> {
        self.store
            .list_recoverable()
            .await?
            .into_iter()
            .map(Job::from_row)
            .collect()
    }

    /// List jobs scoped to one session. Hits the `idx_jobs_session`
    /// index instead of scanning the full table. Newest first.
    pub async fn list_by_session(
        &self,
        session_id: &aura_model::SessionId,
        status: Option<JobStatusKind>,
    ) -> Result<Vec<Job>> {
        let mut jobs = self
            .store
            .list_by_session(session_id)
            .await?
            .into_iter()
            .map(Job::from_row)
            .collect::<Result<Vec<_>>>()?;
        if let Some(k) = status {
            jobs.retain(|j| j.status.kind() == k);
        }
        jobs.sort_by_key(|b| std::cmp::Reverse(b.created_at));
        Ok(jobs)
    }

    /// Non-terminal jobs for one session, status-filtered at the store rather
    /// than loaded whole and retained. Used by `/stop` to find the in-flight
    /// turn + background children without a full-history load on a long-lived
    /// session.
    pub async fn list_active_by_session(
        &self,
        session_id: &aura_model::SessionId,
    ) -> Result<Vec<Job>> {
        self.store
            .list_active_by_session(session_id)
            .await?
            .into_iter()
            .map(Job::from_row)
            .collect()
    }

    /// Non-terminal jobs for one session that represent user-visible
    /// turn activity. This centralizes the `Job::is_turn` filter so
    /// callers such as `/stop`, crash recovery, and TurnState projection
    /// do not each re-define "active reply" for themselves. A `/compact`
    /// runs `Maintenance`-shaped, so it is correctly excluded here even
    /// though its input is a `UserChat` payload.
    pub async fn list_active_turns_by_session(
        &self,
        session_id: &aura_model::SessionId,
    ) -> Result<Vec<Job>> {
        Ok(self
            .list_active_by_session(session_id)
            .await?
            .into_iter()
            .filter(|j| j.is_turn())
            .collect())
    }

    /// When the session has a turn in flight (a non-terminal turn-shaped
    /// job — see [`Job::is_turn`]), the instant it started. Chat
    /// surfaces use this to tell a late-joining client "a reply is being
    /// produced since T"; `None` means the session is idle. A still-
    /// `Pending` turn reports its `created_at` (queued counts as in
    /// flight from the user's point of view).
    pub async fn active_turn_started_at(
        &self,
        session_id: &aura_model::SessionId,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
        let jobs = self.list_active_turns_by_session(session_id).await?;
        Ok(jobs
            .into_iter()
            .map(|j| j.started_at.unwrap_or(j.created_at))
            .max())
    }

    /// Direct children of `parent_job_id` (one level). Used by `/stop`'s
    /// subtree walk to stamp `UserStopped` on (and back-stop the cancellation
    /// of) in-flight descendant jobs such as foreground subagents. Foreground
    /// children are descendants of the turn's loop cancel token, so cancelling
    /// the turn job already cascades into them — this walk wins the
    /// `UserStopped`-vs-`ParentCancelled` reason race and guards any future
    /// child that re-anchors off the actor token.
    pub async fn list_children(&self, parent_job_id: &JobId) -> Result<Vec<Job>> {
        self.store
            .list_children(parent_job_id)
            .await?
            .into_iter()
            .map(Job::from_row)
            .collect()
    }

    pub async fn get_history(&self, job_id: &JobId) -> Result<Vec<JobTransition>> {
        self.store
            .get_transitions(job_id)
            .await?
            .into_iter()
            .map(JobTransition::from_row)
            .collect()
    }

    async fn apply<F>(&self, job_id: &JobId, f: F) -> Result<(Job, JobTransition)>
    where
        F: FnOnce(&mut Job) -> Result<JobTransition>,
    {
        let mut job = self.load_job(job_id).await?;
        let transition = f(&mut job)?;
        Ok((job, transition))
    }

    async fn persist(&self, job: Job, transition: JobTransition) -> Result<()> {
        self.store.save(&job.to_row()?).await?;
        self.store.record_transition(&transition.to_row()?).await?;
        Ok(())
    }

    /// Persist a transition, then fire its lifecycle event on the
    /// broadcast bus. Publish happens **after** the store write succeeds
    /// so a subscriber that races back into the lifecycle (e.g. to look
    /// the job up by id, or to recompute `active_turn_started_at`) is
    /// guaranteed to see the new row. Send errors are dropped on the
    /// floor — broadcast `send` only fails when there are no subscribers,
    /// which is the normal state for a process with no live chat tab and
    /// no in-flight subagent.
    async fn persist_and_publish(
        &self,
        job: Job,
        transition: JobTransition,
        phase: JobPhase,
    ) -> Result<()> {
        let event = JobLifecycleEvent {
            job_id: job.id,
            session_id: job.session_id.clone(),
            parent_job_id: job.parent_job_id,
            phase,
        };
        self.persist(job, transition).await?;
        let _ = self.lifecycle_events.send(event);
        Ok(())
    }

    async fn load_job(&self, job_id: &JobId) -> Result<Job> {
        self.store
            .get(job_id)
            .await?
            .map(Job::from_row)
            .transpose()?
            .ok_or_else(|| JobError::NotFound(format!("job {job_id}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MemoryJobStore;
    use aura_model::{BackgroundCompressionPayload, ContentBlock};

    fn user_chat_input() -> JobInput {
        JobInput::UserChat {
            content: vec![ContentBlock::Text("hi".into())],
        }
    }

    fn dummy_output() -> crate::JobOutput {
        crate::JobOutput::Message {
            content: vec![ContentBlock::Text("ok".into())],
        }
    }

    fn make_lifecycle() -> JobLifecycle {
        JobLifecycle::new(Arc::new(MemoryJobStore::new()))
    }

    #[tokio::test]
    async fn full_success_path() {
        let lc = make_lifecycle();
        let job = lc
            .start_job(
                SessionId::from("s1"),
                TriggerKind::User,
                JobShape::Turn,
                user_chat_input(),
                None,
            )
            .await
            .unwrap();
        assert!(matches!(job.status, JobStatus::Pending));

        lc.start(&job.id).await.unwrap();
        lc.complete(&job.id, dummy_output()).await.unwrap();

        let history = lc.get_history(&job.id).await.unwrap();
        assert_eq!(history.len(), 2);
        assert!(matches!(history[1].to, JobStatus::Completed));
    }

    #[tokio::test]
    async fn cancel_trips_registered_token_and_unregisters_via_guard() {
        let lc = make_lifecycle();
        let job = lc
            .start_job(
                SessionId::from("s1"),
                TriggerKind::User,
                JobShape::Turn,
                user_chat_input(),
                None,
            )
            .await
            .unwrap();
        lc.start(&job.id).await.unwrap();

        let token = CancellationToken::new();
        let guard = lc.register_running(job.id, token.clone());
        assert!(!token.is_cancelled());

        // External cancel observes the token before flipping the row.
        lc.cancel(&job.id, CancelReason::UserPreempt, vec![])
            .await
            .unwrap();
        assert!(token.is_cancelled(), "registered token must be tripped");

        // Drop the guard; subsequent cancels for the same job_id find
        // no live token (the registry is empty).
        drop(guard);
        let unrelated_token = CancellationToken::new();
        // Cancelling an already-terminal job is rejected by the state
        // machine, but we just want to assert the registry didn't keep
        // the entry alive past the guard's drop.
        let job2 = lc
            .start_job(
                SessionId::from("s2"),
                TriggerKind::User,
                JobShape::Turn,
                user_chat_input(),
                None,
            )
            .await
            .unwrap();
        lc.start(&job2.id).await.unwrap();
        let _job2_guard = lc.register_running(job2.id, unrelated_token.clone());
        // Cancelling job (already terminal) should not trip job2's token.
        let _ = lc.cancel(&job.id, CancelReason::UserPreempt, vec![]).await;
        assert!(
            !unrelated_token.is_cancelled(),
            "cancel of unrelated job_id must not trip our token"
        );
    }

    #[tokio::test]
    async fn origin_recorded_as_is_for_any_input_under_any_trigger() {
        // origin is the session's root trigger, recorded honestly and
        // independent of the input payload. The subagent dispatch path
        // relies on a Spawned input working under any inherited root
        // trigger; more broadly, no input/trigger pairing is rejected.
        let lc = make_lifecycle();
        for trigger in [TriggerKind::User, TriggerKind::Cron, TriggerKind::System] {
            let job = lc
                .start_job(
                    SessionId::from(format!("child-of-{trigger:?}")),
                    trigger,
                    JobShape::Turn,
                    JobInput::Spawned {
                        initial_prompt: vec![ContentBlock::Text("task".into())],
                    },
                    None,
                )
                .await
                .expect("any input is allowed under any root trigger");
            assert_eq!(job.origin, trigger);
            assert_eq!(job.input_kind(), crate::JobInputKind::Spawned);
        }
    }

    #[tokio::test]
    async fn user_chat_input_under_cron_session_records_cron_origin() {
        // Input kind and origin are recorded side by side and independently:
        // a UserChat payload in a Cron-trigger session yields input kind =
        // UserChat, origin = Cron, with no payload/trigger constraint.
        let lc = make_lifecycle();
        let job = lc
            .start_job(
                SessionId::from("cron-session"),
                TriggerKind::Cron,
                JobShape::Turn,
                JobInput::UserChat {
                    content: vec![ContentBlock::Text("hi".into())],
                },
                None,
            )
            .await
            .expect("input no longer constrained by trigger");
        assert_eq!(job.origin, TriggerKind::Cron);
        assert_eq!(job.input_kind(), crate::JobInputKind::UserChat);
    }

    #[tokio::test]
    async fn active_turns_exclude_system_jobs() {
        let lc = make_lifecycle();
        let session_id = SessionId::from("s1");
        let turn = lc
            .start_job(
                session_id.clone(),
                TriggerKind::User,
                JobShape::Turn,
                user_chat_input(),
                None,
            )
            .await
            .unwrap();
        lc.start(&turn.id).await.unwrap();
        let system = lc
            .start_job(
                session_id.clone(),
                TriggerKind::System,
                JobShape::Maintenance,
                JobInput::System {
                    payload: BackgroundCompressionPayload { up_to_ordinal: 7 },
                },
                None,
            )
            .await
            .unwrap();
        lc.start(&system.id).await.unwrap();

        let all_active = lc.list_active_by_session(&session_id).await.unwrap();
        assert_eq!(all_active.len(), 2);

        let turns = lc.list_active_turns_by_session(&session_id).await.unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].id, turn.id);
        assert_eq!(
            lc.active_turn_started_at(&session_id).await.unwrap(),
            turns[0].started_at
        );
    }

    #[tokio::test]
    async fn cancel_on_terminal_job_is_noop() {
        let lc = make_lifecycle();
        let job = lc
            .start_job(
                SessionId::from("s1"),
                TriggerKind::User,
                JobShape::Turn,
                user_chat_input(),
                None,
            )
            .await
            .unwrap();
        lc.start(&job.id).await.unwrap();
        lc.complete(&job.id, dummy_output()).await.unwrap();
        // Race: an admin cancel arriving after natural completion must
        // not return InvalidTransition — it's a benign "already done".
        lc.cancel(&job.id, CancelReason::OperatorCancel, vec![])
            .await
            .expect("cancel of terminal job is no-op");
        let loaded = lc.get(&job.id).await.unwrap().unwrap();
        assert!(matches!(loaded.status, JobStatus::Completed));
    }

    #[tokio::test]
    async fn cancel_with_partial_artifacts() {
        let lc = make_lifecycle();
        let job = lc
            .start_job(
                SessionId::from("s1"),
                TriggerKind::User,
                JobShape::Turn,
                user_chat_input(),
                None,
            )
            .await
            .unwrap();
        lc.start(&job.id).await.unwrap();
        let span = SpanId::new();
        lc.cancel(&job.id, CancelReason::UserPreempt, vec![span])
            .await
            .unwrap();
        let loaded = lc.get(&job.id).await.unwrap().unwrap();
        match loaded.status {
            JobStatus::Cancelled {
                reason,
                partial_artifacts,
            } => {
                assert_eq!(reason, CancelReason::UserPreempt);
                assert_eq!(partial_artifacts, vec![span]);
            }
            _ => panic!("expected Cancelled"),
        }
    }
}
