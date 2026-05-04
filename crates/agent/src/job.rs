//! `JobLifecycle` — thin orchestration layer over `JobStore` for the
//! Job state machine. Replaces the legacy `JobManager` /
//! `ObservabilityRecorder`. See `docs/modules/job.md` for the design.

use std::sync::Arc;

#[cfg(test)]
use aura_job::JobStatus;
use aura_job::{CancelReason, Job, JobError, JobInput, JobStatusKind, JobTransition};
use aura_model::{JobId, SessionId, SpanId, TriggerKind};
use aura_storage::JobStore;
use tokio_util::sync::CancellationToken;

use crate::cancel::{JobCancellationGuard, JobCancellationRegistry};

type Result<T> = std::result::Result<T, JobError>;

/// Owns the job state machine + persistence orchestration. Pure
/// passthrough to `Job`'s internal transition validation —
/// `JobLifecycle` itself contains no state machine logic.
pub struct JobLifecycle {
    store: Arc<dyn JobStore>,
    /// Process-wide registry of `JobId → CancellationToken` for
    /// in-flight jobs. `cancel()` trips the matching token (if any)
    /// in addition to flipping the DB row. See `agent::cancel`.
    cancellation: Arc<JobCancellationRegistry>,
}

impl JobLifecycle {
    pub fn new(store: Arc<dyn JobStore>) -> Self {
        Self::with_cancellation(store, Arc::new(JobCancellationRegistry::new()))
    }

    pub fn with_cancellation(
        store: Arc<dyn JobStore>,
        cancellation: Arc<JobCancellationRegistry>,
    ) -> Self {
        Self {
            store,
            cancellation,
        }
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

    /// Create a new job in `Pending`. The caller chooses the soul
    /// version (typically `Session.bound_soul_version`, or whatever
    /// the live agent has actually loaded if drift is being recorded
    /// upstream).
    ///
    /// `session_trigger_kind` is the root trigger kind of the owning
    /// session — used to enforce the `JobKind ↔ TriggerKind` invariant
    /// documented in `aura_job::kind`. Mismatches return
    /// `JobError::KindMismatch` with a descriptive message (rather than
    /// passing through silently as before).
    pub async fn start_job(
        &self,
        session_id: SessionId,
        session_trigger_kind: TriggerKind,
        input: JobInput,
        effective_soul_version: impl Into<String>,
        parent_job_id: Option<JobId>,
    ) -> Result<Job> {
        let job_kind = input.kind();
        if !job_kind.allowed_for(session_trigger_kind) {
            return Err(JobError::KindMismatch(format!(
                "job kind {job_kind:?} not allowed in {session_trigger_kind:?}-trigger session \
                 (see aura_job::kind allowed-for table)"
            )));
        }
        let job = Job::new(session_id, input, effective_soul_version, parent_job_id);
        self.store.create(&job).await?;
        Ok(job)
    }

    /// Move `Pending → InProgress`.
    pub async fn start(&self, job_id: &JobId) -> Result<()> {
        let (job, transition) = self.apply(job_id, |j| j.start()).await?;
        self.persist(job, transition).await
    }

    /// Move `InProgress → Completed` with a final output.
    pub async fn complete(&self, job_id: &JobId, output: aura_job::JobOutput) -> Result<()> {
        let (job, transition) = self.apply(job_id, |j| j.complete(output)).await?;
        self.persist(job, transition).await
    }

    /// Move to `Failed { reason }`.
    pub async fn fail(&self, job_id: &JobId, reason: impl Into<String>) -> Result<()> {
        let reason = reason.into();
        let (job, transition) = self.apply(job_id, |j| j.fail(&reason)).await?;
        self.persist(job, transition).await
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
        self.persist(job, transition).await
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
        self.store.get(job_id).await
    }

    /// List jobs, optionally filtered by status discriminator. Newest
    /// `created_at` first.
    pub async fn list(&self, status: Option<JobStatusKind>) -> Result<Vec<Job>> {
        let mut jobs = match status {
            Some(k) => self.store.list_by_status_kind(k).await?,
            None => self.store.list_all().await?,
        };
        jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(jobs)
    }

    /// List every non-terminal job (`Pending` / `InProgress` / `Stuck`)
    /// in one query. One trip to the store instead of three —
    /// `QueryApi::find_recoverable_jobs` used to call `list(Some(k))`
    /// for each kind serially.
    pub async fn list_recoverable(&self) -> Result<Vec<Job>> {
        self.store.list_recoverable().await
    }

    /// List jobs scoped to one session. Hits the `idx_jobs_session`
    /// index instead of scanning the full table. Newest first.
    pub async fn list_by_session(
        &self,
        session_id: &aura_model::SessionId,
        status: Option<JobStatusKind>,
    ) -> Result<Vec<Job>> {
        let mut jobs = self.store.list_by_session(session_id).await?;
        if let Some(k) = status {
            jobs.retain(|j| j.status.kind() == k);
        }
        jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(jobs)
    }

    pub async fn get_history(&self, job_id: &JobId) -> Result<Vec<JobTransition>> {
        self.store.get_transitions(job_id).await
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
        self.store.save(&job).await?;
        self.store.record_transition(&transition).await?;
        Ok(())
    }

    async fn load_job(&self, job_id: &JobId) -> Result<Job> {
        self.store
            .get(job_id)
            .await?
            .ok_or_else(|| JobError::NotFound(format!("job {job_id}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::ContentBlock;
    use aura_storage::test_support::MemoryJobStore;

    fn user_chat_input() -> JobInput {
        JobInput::UserChat {
            content: vec![ContentBlock::Text("hi".into())],
        }
    }

    fn dummy_output() -> aura_job::JobOutput {
        aura_job::JobOutput::Message {
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
                "soul-v1",
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
                "soul-v1",
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
                "soul-v1",
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
                    "soul-v1",
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
                "soul-v1",
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
                "soul-v1",
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
                "soul-v1",
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
