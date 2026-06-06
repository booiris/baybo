//! `JobLifecycle` — thin orchestration layer over `JobStore` for the
//! Job state machine. See `docs/modules/job.md` for the design.

use std::sync::Arc;

#[cfg(test)]
use crate::JobStatus;
use crate::cancellation_registry::{JobCancellationGuard, JobCancellationRegistry};
use crate::{CancelReason, Job, JobError, JobInput, JobStatusKind, JobTransition};
use aura_model::{JobId, SessionId, SpanId, TriggerKind};
use aura_store::JobStore;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

type Result<T> = std::result::Result<T, JobError>;

/// Process-wide capacity for the terminal-event broadcast bus.
///
/// Sized to absorb the burst a multi-job subagent fan-out can produce
/// without lagging the parent's wait loop. Subscribers that lag still
/// reconcile against `list_by_session` so dropped events don't cause
/// lost terminations — capacity is a latency knob, not correctness.
const JOB_TERMINAL_EVENT_CAPACITY: usize = 256;

/// Terminal-state notification published by `JobLifecycle` whenever a
/// job transitions to `Completed`, `Failed`, or `Cancelled`. Carries
/// the minimum identifiers a subscriber needs to filter without going
/// back to the store: the terminating `JobId`, its session, and the
/// optional parent for hierarchy-scoped waits (subagent path).
///
/// `kind` is always one of the three terminal `JobStatusKind`
/// discriminants — non-terminal transitions (`start`, `stuck`,
/// `recover`) are not published.
#[derive(Debug, Clone)]
pub struct JobTerminalEvent {
    pub job_id: JobId,
    pub session_id: SessionId,
    pub parent_job_id: Option<JobId>,
    pub kind: JobStatusKind,
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
    /// Fire-and-forget bus for terminal transitions. The subagent
    /// runtime subscribes to this so the parent unblocks on the
    /// child's `Completed` / `Failed` / `Cancelled` regardless of
    /// whether the child also produced an `AgentEvent::Message`.
    terminal_events: broadcast::Sender<JobTerminalEvent>,
}

impl JobLifecycle {
    pub fn new(store: Arc<dyn JobStore>) -> Self {
        let (terminal_events, _rx) = broadcast::channel(JOB_TERMINAL_EVENT_CAPACITY);
        Self {
            store,
            cancellation: Arc::new(JobCancellationRegistry::new()),
            terminal_events,
        }
    }

    /// Subscribe to terminal-state events. Subscribers must reconcile
    /// against the store (e.g. `list_by_session`) on
    /// `broadcast::error::RecvError::Lagged` — a dropped event is not
    /// re-published.
    pub fn subscribe_terminal_events(&self) -> broadcast::Receiver<JobTerminalEvent> {
        self.terminal_events.subscribe()
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
    /// `session_trigger_kind` is the root trigger kind of the owning
    /// session — used to enforce the `JobKind ↔ TriggerKind` invariant
    /// documented in `aura_job::kind`. Mismatches return
    /// `JobError::KindMismatch` with a descriptive message (rather than
    /// passing through silently as before).
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
        session_trigger_kind: TriggerKind,
        input: JobInput,
        parent_job_id: Option<JobId>,
    ) -> Result<Job> {
        let job_kind = input.kind();
        if !job_kind.allowed_for(session_trigger_kind) {
            return Err(JobError::KindMismatch(format!(
                "job kind {job_kind:?} not allowed in {session_trigger_kind:?}-trigger session \
                 (see aura_job::kind allowed-for table)"
            )));
        }
        let job = Job::new(session_id, input, parent_job_id);
        self.store.create(&job.to_row()?).await?;
        Ok(job)
    }

    /// Move `Pending → InProgress`.
    pub async fn start(&self, job_id: &JobId) -> Result<()> {
        let (job, transition) = self.apply(job_id, |j| j.start()).await?;
        self.persist(job, transition).await
    }

    /// Move `InProgress → Completed` with a final output.
    pub async fn complete(&self, job_id: &JobId, output: crate::JobOutput) -> Result<()> {
        let (job, transition) = self.apply(job_id, |j| j.complete(output)).await?;
        self.persist_and_publish(job, transition).await
    }

    /// Move to `Failed { reason }`.
    pub async fn fail(&self, job_id: &JobId, reason: impl Into<String>) -> Result<()> {
        let reason = reason.into();
        let (job, transition) = self.apply(job_id, |j| j.fail(&reason)).await?;
        self.persist_and_publish(job, transition).await
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
        self.persist_and_publish(job, transition).await
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
        self.persist_and_publish(job, transition).await
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

    /// Persist a terminal transition, then fire the matching event on
    /// the broadcast bus. Publish happens **after** the store write
    /// succeeds so a subscriber that races back into the lifecycle
    /// (e.g. to look the job up by id) is guaranteed to see the
    /// terminal row. Send errors are dropped on the floor — broadcast
    /// `send` only fails when there are no subscribers, which is the
    /// normal state for non-subagent jobs.
    async fn persist_and_publish(&self, job: Job, transition: JobTransition) -> Result<()> {
        let event = JobTerminalEvent {
            job_id: job.id,
            session_id: job.session_id.clone(),
            parent_job_id: job.parent_job_id,
            kind: job.status.kind(),
        };
        self.persist(job, transition).await?;
        let _ = self.terminal_events.send(event);
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
    use aura_model::ContentBlock;

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
    async fn spawned_job_kind_allowed_under_every_root_trigger() {
        // Pins the contract that the subagent dispatch path relies on:
        // JobInput::Spawned must work for a child session whose root
        // trigger is inherited from the parent (User / Cron / System).
        let lc = make_lifecycle();
        for trigger in [TriggerKind::User, TriggerKind::Cron, TriggerKind::System] {
            let job = lc
                .start_job(
                    SessionId::from(format!("child-of-{trigger:?}")),
                    trigger,
                    JobInput::Spawned {
                        initial_prompt: vec![ContentBlock::Text("task".into())],
                    },
                    None,
                )
                .await;
            assert!(
                job.is_ok(),
                "subagent dispatch under {trigger:?} root must be allowed: {:?}",
                job.err()
            );
        }
    }

    #[tokio::test]
    async fn user_chat_under_cron_session_is_rejected() {
        // The bug this guards against: subagent.rs used to send
        // AgentMessage::UserInput, which mapped to JobInput::UserChat —
        // and a Cron-rooted child session rejected that with KindMismatch.
        // The fix routes subagents through JobInput::Spawned (covered by
        // the test above); this test pins the underlying state-machine
        // rejection so a regression elsewhere can't silently restore the
        // old shape.
        let lc = make_lifecycle();
        let err = lc
            .start_job(
                SessionId::from("cron-session"),
                TriggerKind::Cron,
                JobInput::UserChat {
                    content: vec![ContentBlock::Text("hi".into())],
                },
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, JobError::KindMismatch(_)));
    }

    #[tokio::test]
    async fn cancel_on_terminal_job_is_noop() {
        let lc = make_lifecycle();
        let job = lc
            .start_job(
                SessionId::from("s1"),
                TriggerKind::User,
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
