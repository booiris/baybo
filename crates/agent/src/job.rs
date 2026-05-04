//! `JobLifecycle` — thin orchestration layer over `JobStore` for the
//! Job state machine. Replaces the legacy `JobManager` /
//! `ObservabilityRecorder`. See `docs/modules/job.md` for the design.

use std::sync::Arc;

#[cfg(test)]
use aura_job::JobStatus;
use aura_job::{CancelReason, Job, JobError, JobInput, JobStatusKind, JobTransition};
use aura_model::{JobId, SessionId, SpanId, TriggerKind};
use aura_storage::JobStore;

type Result<T> = std::result::Result<T, JobError>;

/// Owns the job state machine + persistence orchestration. Pure
/// passthrough to `Job`'s internal transition validation —
/// `JobLifecycle` itself contains no state machine logic.
pub struct JobLifecycle {
    store: Arc<dyn JobStore>,
}

impl JobLifecycle {
    pub fn new(store: Arc<dyn JobStore>) -> Self {
        Self { store }
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
    pub async fn cancel(
        &self,
        job_id: &JobId,
        reason: CancelReason,
        partial_artifacts: Vec<SpanId>,
    ) -> Result<()> {
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

    /// Recover non-terminal jobs after a process restart.
    ///
    /// `InProgress` jobs become `Stuck { reason: "system restart..." }`
    /// (their executing context was lost). Other non-terminal statuses
    /// are left unchanged. Callers (typically `main`) then decide
    /// whether to `recover()` or `fail()` each `Stuck` job.
    ///
    /// Returns the count of jobs that actually transitioned.
    pub async fn recover_interrupted(&self) -> Result<usize> {
        let mut recovered = 0;
        for mut job in self.store.list_recoverable().await? {
            if let Some(transition) = job.mark_interrupted()? {
                self.persist(job, transition).await?;
                recovered += 1;
            }
        }
        Ok(recovered)
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

    #[tokio::test]
    async fn recover_interrupted_moves_in_progress_to_stuck() {
        let lc = make_lifecycle();
        // Pending — left alone
        lc.start_job(
            SessionId::from("s1"),
            TriggerKind::User,
            user_chat_input(),
            "soul-v1",
            None,
        )
        .await
        .unwrap();
        // InProgress — should become Stuck
        let in_progress = lc
            .start_job(
                SessionId::from("s1"),
                TriggerKind::User,
                user_chat_input(),
                "soul-v1",
                None,
            )
            .await
            .unwrap();
        lc.start(&in_progress.id).await.unwrap();
        // Completed — left alone
        let completed = lc
            .start_job(
                SessionId::from("s1"),
                TriggerKind::User,
                user_chat_input(),
                "soul-v1",
                None,
            )
            .await
            .unwrap();
        lc.start(&completed.id).await.unwrap();
        lc.complete(&completed.id, dummy_output()).await.unwrap();

        let count = lc.recover_interrupted().await.unwrap();
        assert_eq!(count, 1);

        let after = lc.get(&in_progress.id).await.unwrap().unwrap();
        assert!(matches!(after.status, JobStatus::Stuck { .. }));
        let still_completed = lc.get(&completed.id).await.unwrap().unwrap();
        assert!(matches!(still_completed.status, JobStatus::Completed));
    }
}
