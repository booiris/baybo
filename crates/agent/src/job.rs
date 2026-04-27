use std::sync::Arc;

use aura_job::{
    AcceptancePolicy, Job, JobError, JobStatus, JobTransition, OperationKind, RecoveryPolicy,
};
use aura_storage::JobStore;

type Result<T> = std::result::Result<T, JobError>;

/// Manages job lifecycle: load from store, apply transition, persist.
///
/// State machine validation and timestamp management live on `Job` itself.
/// This manager is a thin persistence orchestrator.
pub struct JobManager {
    store: Arc<dyn JobStore>,
}

/// Outcome of one `apply_recovery_policy` pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryStats {
    /// Stuck jobs moved to `Abandoned` (policy = Abandon, or AutoResume past max_attempts).
    pub abandoned: usize,
    /// Stuck jobs left unchanged (waiting for an external resumer).
    pub left_stuck: usize,
}

/// Aggregated outcome of `bootstrap_recovery`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BootstrapRecoveryStats {
    /// Number of `InProgress` jobs moved to `Stuck`.
    pub interrupted: usize,
    /// Number of `Auto`-policy jobs forward-filled past `Completed`/`Submitted`.
    pub reconciled: usize,
    /// Outcome of the recovery-policy pass.
    pub recovery: RecoveryStats,
}

impl JobManager {
    pub fn new(store: Arc<dyn JobStore>) -> Self {
        Self { store }
    }

    pub async fn create_job(
        &self,
        session_id: &str,
        kind: OperationKind,
        parent: Option<&str>,
    ) -> Result<Job> {
        let job = Job::new(session_id, kind, parent);
        self.store.create(&job).await?;
        Ok(job)
    }

    pub async fn start(&self, job_id: &str) -> Result<()> {
        let (job, transition) = self.apply(job_id, |j| j.start()).await?;
        self.persist(job, transition).await
    }

    /// Move a job from `InProgress` to `Completed` and apply its
    /// `AcceptancePolicy`. Under the default `Auto` policy the manager
    /// also walks `Completed → Submitted → Accepted` so user-facing
    /// flows never see the verifier seam. `AutoSubmit` stops at
    /// `Submitted`; `Manual` stops at `Completed`.
    ///
    /// All chained transitions are applied to the in-memory `Job` first
    /// and persisted with a single `store.save` so a crash cannot leave
    /// an `Auto` job stranded between `Completed` and `Accepted`. The
    /// per-transition records are appended afterwards; if appending one
    /// fails the job is already in its final state and `reconcile_auto_chains`
    /// will not need to act, only the audit trail loses a row.
    pub async fn complete(&self, job_id: &str, output: serde_json::Value) -> Result<()> {
        let mut job = self.load_job(job_id).await?;
        let mut transitions = Vec::with_capacity(3);
        transitions.push(job.complete(output)?);
        match job.acceptance.clone() {
            AcceptancePolicy::Auto => {
                transitions.push(job.submit()?);
                transitions.push(job.accept()?);
            }
            AcceptancePolicy::AutoSubmit { .. } => {
                transitions.push(job.submit()?);
            }
            AcceptancePolicy::Manual { .. } => {}
        }
        self.store.save(&job).await?;
        for t in transitions {
            self.store.record_transition(&t).await?;
        }
        Ok(())
    }

    /// Move a job from `Completed` to `Submitted`. Use this when the
    /// job's `AcceptancePolicy` is `AutoSubmit` or `Manual` and an
    /// external submitter is signalling that the agent's output is
    /// ready for verification.
    pub async fn submit(&self, job_id: &str) -> Result<()> {
        let (job, transition) = self.apply(job_id, |j| j.submit()).await?;
        self.persist(job, transition).await
    }

    /// Move a job from `Submitted` to `Accepted`. Used by Manual /
    /// AutoSubmit acceptors (user, validator, or the timeout
    /// scheduler) to record a positive verdict.
    pub async fn accept(&self, job_id: &str) -> Result<()> {
        let (job, transition) = self.apply(job_id, |j| j.accept()).await?;
        self.persist(job, transition).await
    }

    pub async fn fail(&self, job_id: &str, error: &str) -> Result<()> {
        let (job, transition) = self.apply(job_id, |j| j.fail(error)).await?;
        self.persist(job, transition).await
    }

    /// Look up a job by id.
    pub async fn get(&self, job_id: &str) -> Result<Option<Job>> {
        self.store.get(job_id).await
    }

    /// List jobs, optionally filtered by status. When `status` is `None`
    /// every persisted job is returned; otherwise only those matching.
    /// Ordering: newest `created_at` first.
    pub async fn list(&self, status: Option<JobStatus>) -> Result<Vec<Job>> {
        let mut jobs = match status {
            Some(s) => self.store.list_by_status(s).await?,
            None => self.store.list_all().await?,
        };
        jobs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(jobs)
    }

    /// Cancel a job. `Pending`, `InProgress`, and `Stuck` jobs are
    /// transitioned directly to `Cancelled`; jobs that already reached
    /// `Completed` or beyond are rejected so a settled audit trail isn't
    /// reclassified after the fact.
    pub async fn cancel(&self, job_id: &str) -> Result<Job> {
        let reason = "cancelled by operator";
        let (job, transition) = self.apply(job_id, |j| j.cancel(reason)).await?;
        self.persist(job, transition).await?;
        self.load_job(job_id).await
    }

    /// Run the full startup recovery sequence in the only correct order:
    ///
    /// 1. `recover_interrupted` (`InProgress → Stuck` for jobs cut short
    ///    by a process exit).
    /// 2. `reconcile_auto_chains` (forward-fill `Auto`-policy jobs that
    ///    were stranded mid-acceptance — only happens for legacy data
    ///    or a partially-applied save; `complete()` itself is now
    ///    crash-safe).
    /// 3. `apply_recovery_policy` (move `Stuck` jobs to `Abandoned` if
    ///    their policy says so).
    ///
    /// Returns each step's result tuple.
    pub async fn bootstrap_recovery(&self) -> Result<BootstrapRecoveryStats> {
        let interrupted = self.recover_interrupted().await?;
        let reconciled = self.reconcile_auto_chains().await?;
        let recovery = self.apply_recovery_policy().await?;
        Ok(BootstrapRecoveryStats {
            interrupted,
            reconciled,
            recovery,
        })
    }

    /// Forward-fill `Auto`-policy jobs whose acceptance chain stopped
    /// short. Drives `Completed → Submitted → Accepted` and
    /// `Submitted → Accepted` for jobs whose `acceptance` is `Auto`,
    /// using a single in-memory transition batch per job.
    ///
    /// `AutoSubmit` and `Manual` jobs in those states are intentional —
    /// they are waiting for an external acceptor — and are left alone.
    pub async fn reconcile_auto_chains(&self) -> Result<usize> {
        let mut moved = 0usize;
        for status in [JobStatus::Completed, JobStatus::Submitted] {
            let jobs = self.store.list_by_status(status.clone()).await?;
            for mut job in jobs {
                if !matches!(job.acceptance, AcceptancePolicy::Auto) {
                    continue;
                }
                let mut transitions = Vec::with_capacity(2);
                if job.status == JobStatus::Completed {
                    transitions.push(job.submit()?);
                }
                if job.status == JobStatus::Submitted {
                    transitions.push(job.accept()?);
                }
                if transitions.is_empty() {
                    continue;
                }
                self.store.save(&job).await?;
                for t in transitions {
                    self.store.record_transition(&t).await?;
                }
                moved += 1;
            }
        }
        Ok(moved)
    }

    /// Apply each `Stuck` job's `RecoveryPolicy`. Jobs whose policy
    /// is `Abandon` (or `AutoResume` that has exceeded `max_attempts`)
    /// are moved to `Abandoned`; the rest are left for an external
    /// resumer to drive `Stuck → InProgress` when ready.
    ///
    /// Intended to run after `recover_interrupted` on startup, then
    /// periodically thereafter; or use `bootstrap_recovery` to chain
    /// the canonical sequence.
    pub async fn apply_recovery_policy(&self) -> Result<RecoveryStats> {
        let stuck = self.store.list_by_status(JobStatus::Stuck).await?;
        let mut stats = RecoveryStats::default();
        for mut job in stuck {
            let abandon_reason = match &job.recovery {
                RecoveryPolicy::Abandon => Some("recovery policy: abandon".to_string()),
                RecoveryPolicy::AutoResume { max_attempts }
                    if job.recovery_attempts >= *max_attempts =>
                {
                    Some(format!("max recovery attempts ({max_attempts}) reached"))
                }
                _ => None,
            };
            match abandon_reason {
                Some(reason) => {
                    let t = job.abandon(&reason)?;
                    self.persist(job, t).await?;
                    stats.abandoned += 1;
                }
                None => stats.left_stuck += 1,
            }
        }
        Ok(stats)
    }

    /// Recover jobs that were interrupted by a system restart.
    ///
    /// Scans all non-terminal jobs and calls `mark_interrupted()` on each.
    /// `InProgress` jobs are moved to `Stuck`; others are left unchanged.
    /// Returns the number of jobs that were transitioned.
    pub async fn recover_interrupted(&self) -> Result<usize> {
        let statuses = [
            JobStatus::Pending,
            JobStatus::InProgress,
            JobStatus::Completed,
            JobStatus::Submitted,
            JobStatus::Stuck,
        ];

        let mut recovered = 0;
        for status in statuses {
            let jobs = self.store.list_by_status(status).await?;
            for mut job in jobs {
                if let Some(transition) = job.mark_interrupted()? {
                    self.persist(job, transition).await?;
                    recovered += 1;
                }
            }
        }
        Ok(recovered)
    }

    /// Load a job, apply a transition closure, return the mutated job and transition.
    async fn apply<F>(&self, job_id: &str, f: F) -> Result<(Job, JobTransition)>
    where
        F: FnOnce(&mut Job) -> Result<JobTransition>,
    {
        let mut job = self.load_job(job_id).await?;
        let transition = f(&mut job)?;
        Ok((job, transition))
    }

    /// Persist the updated job and its transition record.
    async fn persist(&self, job: Job, transition: JobTransition) -> Result<()> {
        self.store.save(&job).await?;
        self.store.record_transition(&transition).await?;
        Ok(())
    }

    async fn load_job(&self, job_id: &str) -> Result<Job> {
        self.store
            .get(job_id)
            .await?
            .ok_or_else(|| JobError::NotFound(format!("job {job_id}")))
    }
}

#[cfg(test)]
impl JobManager {
    async fn stuck(&self, job_id: &str, reason: &str) -> Result<()> {
        let (job, transition) = self.apply(job_id, |j| j.stuck(reason)).await?;
        self.persist(job, transition).await
    }

    async fn recover(&self, job_id: &str, reason: &str) -> Result<()> {
        let (job, transition) = self.apply(job_id, |j| j.recover(reason)).await?;
        self.persist(job, transition).await
    }

    async fn get_history(&self, job_id: &str) -> Result<Vec<JobTransition>> {
        self.store.get_transitions(job_id).await
    }

    async fn set_acceptance(&self, job_id: &str, acceptance: AcceptancePolicy) -> Result<()> {
        let mut job = self.load_job(job_id).await?;
        job.acceptance = acceptance;
        self.store.save(&job).await?;
        Ok(())
    }

    async fn set_recovery(&self, job_id: &str, recovery: RecoveryPolicy) -> Result<()> {
        let mut job = self.load_job(job_id).await?;
        job.recovery = recovery;
        self.store.save(&job).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use parking_lot::Mutex;

    struct InMemoryJobStore {
        jobs: Mutex<Vec<Job>>,
        transitions: Mutex<Vec<JobTransition>>,
    }

    impl InMemoryJobStore {
        fn new() -> Self {
            Self {
                jobs: Mutex::new(Vec::new()),
                transitions: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl JobStore for InMemoryJobStore {
        async fn create(&self, job: &Job) -> Result<()> {
            self.jobs.lock().push(job.clone());
            Ok(())
        }

        async fn get(&self, job_id: &str) -> Result<Option<Job>> {
            Ok(self.jobs.lock().iter().find(|j| j.id == job_id).cloned())
        }

        async fn save(&self, job: &Job) -> Result<()> {
            let mut jobs = self.jobs.lock();
            let stored = jobs
                .iter_mut()
                .find(|j| j.id == job.id)
                .ok_or_else(|| JobError::NotFound(format!("job {}", job.id)))?;
            *stored = job.clone();
            Ok(())
        }

        async fn list_by_session(&self, session_id: &str) -> Result<Vec<Job>> {
            Ok(self
                .jobs
                .lock()
                .iter()
                .filter(|j| j.session_id == session_id)
                .cloned()
                .collect())
        }

        async fn list_by_status(&self, status: JobStatus) -> Result<Vec<Job>> {
            Ok(self
                .jobs
                .lock()
                .iter()
                .filter(|j| j.status == status)
                .cloned()
                .collect())
        }

        async fn list_children(&self, parent_job_id: &str) -> Result<Vec<Job>> {
            Ok(self
                .jobs
                .lock()
                .iter()
                .filter(|j| j.parent_job_id.as_deref() == Some(parent_job_id))
                .cloned()
                .collect())
        }

        async fn list_all(&self) -> Result<Vec<Job>> {
            Ok(self.jobs.lock().clone())
        }

        async fn record_transition(&self, transition: &JobTransition) -> Result<()> {
            self.transitions.lock().push(transition.clone());
            Ok(())
        }

        async fn get_transitions(&self, job_id: &str) -> Result<Vec<JobTransition>> {
            Ok(self
                .transitions
                .lock()
                .iter()
                .filter(|t| t.job_id == job_id)
                .cloned()
                .collect())
        }
    }

    fn test_kind() -> OperationKind {
        OperationKind::LlmCall {
            model: "test-model".into(),
        }
    }

    fn make_manager() -> JobManager {
        JobManager::new(Arc::new(InMemoryJobStore::new()))
    }

    #[tokio::test]
    async fn auto_acceptance_chains_complete_to_accepted() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        assert_eq!(job.status, JobStatus::Pending);
        assert!(matches!(job.acceptance, AcceptancePolicy::Auto));

        mgr.start(&job.id).await.unwrap();
        mgr.complete(&job.id, serde_json::json!({"ok": true}))
            .await
            .unwrap();

        let final_job = mgr.store.get(&job.id).await.unwrap().unwrap();
        assert_eq!(final_job.status, JobStatus::Accepted);
        assert!(final_job.submitted_at.is_some());
        assert!(final_job.accepted_at.is_some());

        let history = mgr.get_history(&job.id).await.unwrap();
        assert_eq!(history.len(), 4);
        assert_eq!(history[0].to, JobStatus::InProgress);
        assert_eq!(history[1].to, JobStatus::Completed);
        assert_eq!(history[2].to, JobStatus::Submitted);
        assert_eq!(history[3].to, JobStatus::Accepted);
    }

    #[tokio::test]
    async fn auto_submit_acceptance_stops_at_submitted() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.set_acceptance(
            &job.id,
            AcceptancePolicy::AutoSubmit {
                acceptor: aura_job::Acceptor::User,
            },
        )
        .await
        .unwrap();

        mgr.start(&job.id).await.unwrap();
        mgr.complete(&job.id, serde_json::json!(null))
            .await
            .unwrap();

        let after = mgr.store.get(&job.id).await.unwrap().unwrap();
        assert_eq!(after.status, JobStatus::Submitted);
        assert!(after.accepted_at.is_none());

        // External acceptor signs off later.
        mgr.accept(&job.id).await.unwrap();
        let final_job = mgr.store.get(&job.id).await.unwrap().unwrap();
        assert_eq!(final_job.status, JobStatus::Accepted);
    }

    #[tokio::test]
    async fn manual_acceptance_stops_at_completed() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.set_acceptance(
            &job.id,
            AcceptancePolicy::Manual {
                submitter: aura_job::Acceptor::User,
                acceptor: aura_job::Acceptor::User,
            },
        )
        .await
        .unwrap();

        mgr.start(&job.id).await.unwrap();
        mgr.complete(&job.id, serde_json::json!(null))
            .await
            .unwrap();

        let after = mgr.store.get(&job.id).await.unwrap().unwrap();
        assert_eq!(after.status, JobStatus::Completed);
        assert!(after.submitted_at.is_none());

        mgr.submit(&job.id).await.unwrap();
        mgr.accept(&job.id).await.unwrap();
        let final_job = mgr.store.get(&job.id).await.unwrap().unwrap();
        assert_eq!(final_job.status, JobStatus::Accepted);
    }

    #[tokio::test]
    async fn fail_from_in_progress() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.start(&job.id).await.unwrap();
        mgr.fail(&job.id, "timeout").await.unwrap();

        let history = mgr.get_history(&job.id).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].to, JobStatus::Failed);
    }

    #[tokio::test]
    async fn stuck_then_recover() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.start(&job.id).await.unwrap();
        mgr.stuck(&job.id, "no response").await.unwrap();
        mgr.recover(&job.id, "retrying").await.unwrap();
        mgr.complete(&job.id, serde_json::json!(null))
            .await
            .unwrap();

        let final_job = mgr.store.get(&job.id).await.unwrap().unwrap();
        assert_eq!(final_job.status, JobStatus::Accepted);

        // Pending→InProgress, InProgress→Stuck, Stuck→InProgress,
        // InProgress→Completed, Completed→Submitted, Submitted→Accepted.
        let history = mgr.get_history(&job.id).await.unwrap();
        assert_eq!(history.len(), 6);
    }

    #[tokio::test]
    async fn stuck_then_fail() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.start(&job.id).await.unwrap();
        mgr.stuck(&job.id, "hung").await.unwrap();
        mgr.fail(&job.id, "unrecoverable").await.unwrap();

        let history = mgr.get_history(&job.id).await.unwrap();
        assert_eq!(history.last().unwrap().to, JobStatus::Failed);
    }

    #[tokio::test]
    async fn cannot_complete_from_pending() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        let err = mgr
            .complete(&job.id, serde_json::json!(null))
            .await
            .unwrap_err();
        assert!(matches!(err, JobError::InvalidTransition(_)));
    }

    #[tokio::test]
    async fn cannot_accept_from_pending() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        let err = mgr.accept(&job.id).await.unwrap_err();
        assert!(matches!(err, JobError::InvalidTransition(_)));
    }

    #[tokio::test]
    async fn not_found_returns_error() {
        let mgr = make_manager();
        let err = mgr.start("nonexistent").await.unwrap_err();
        assert!(matches!(err, JobError::NotFound(_)));
    }

    #[tokio::test]
    async fn recover_interrupted_moves_in_progress_to_stuck() {
        let mgr = make_manager();

        let pending_job = mgr.create_job("s1", test_kind(), None).await.unwrap();

        let in_progress_job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.start(&in_progress_job.id).await.unwrap();

        // Manual acceptance keeps the job in `Completed` so the test can
        // verify recover_interrupted leaves non-InProgress states alone.
        let completed_job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.set_acceptance(
            &completed_job.id,
            AcceptancePolicy::Manual {
                submitter: aura_job::Acceptor::User,
                acceptor: aura_job::Acceptor::User,
            },
        )
        .await
        .unwrap();
        mgr.start(&completed_job.id).await.unwrap();
        mgr.complete(&completed_job.id, serde_json::json!(null))
            .await
            .unwrap();

        let accepted_job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.start(&accepted_job.id).await.unwrap();
        mgr.complete(&accepted_job.id, serde_json::json!(null))
            .await
            .unwrap();

        let failed_job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.start(&failed_job.id).await.unwrap();
        mgr.fail(&failed_job.id, "err").await.unwrap();

        let count = mgr.recover_interrupted().await.unwrap();
        assert_eq!(count, 1);

        let job = mgr.store.get(&in_progress_job.id).await.unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Stuck);

        let job = mgr.store.get(&pending_job.id).await.unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Pending);

        let job = mgr.store.get(&completed_job.id).await.unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Completed);

        let job = mgr.store.get(&accepted_job.id).await.unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Accepted);

        let job = mgr.store.get(&failed_job.id).await.unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Failed);
    }

    #[tokio::test]
    async fn recover_interrupted_no_jobs_returns_zero() {
        let mgr = make_manager();
        let count = mgr.recover_interrupted().await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn apply_recovery_policy_abandons_when_policy_is_abandon() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.set_recovery(&job.id, RecoveryPolicy::Abandon)
            .await
            .unwrap();
        mgr.start(&job.id).await.unwrap();
        mgr.stuck(&job.id, "hung").await.unwrap();

        let stats = mgr.apply_recovery_policy().await.unwrap();
        assert_eq!(stats.abandoned, 1);
        assert_eq!(stats.left_stuck, 0);

        let final_job = mgr.store.get(&job.id).await.unwrap().unwrap();
        assert_eq!(final_job.status, JobStatus::Abandoned);
    }

    #[tokio::test]
    async fn apply_recovery_policy_abandons_when_max_attempts_exhausted() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.set_recovery(&job.id, RecoveryPolicy::AutoResume { max_attempts: 1 })
            .await
            .unwrap();
        mgr.start(&job.id).await.unwrap();
        mgr.stuck(&job.id, "hung").await.unwrap();
        mgr.recover(&job.id, "first retry").await.unwrap();
        mgr.stuck(&job.id, "hung again").await.unwrap();

        let stats = mgr.apply_recovery_policy().await.unwrap();
        assert_eq!(stats.abandoned, 1);

        let final_job = mgr.store.get(&job.id).await.unwrap().unwrap();
        assert_eq!(final_job.status, JobStatus::Abandoned);
    }

    #[tokio::test]
    async fn apply_recovery_policy_leaves_stuck_when_policy_is_manual() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.set_recovery(&job.id, RecoveryPolicy::Manual)
            .await
            .unwrap();
        mgr.start(&job.id).await.unwrap();
        mgr.stuck(&job.id, "hung").await.unwrap();

        let stats = mgr.apply_recovery_policy().await.unwrap();
        assert_eq!(stats.abandoned, 0);
        assert_eq!(stats.left_stuck, 1);

        let still_stuck = mgr.store.get(&job.id).await.unwrap().unwrap();
        assert_eq!(still_stuck.status, JobStatus::Stuck);
    }

    #[tokio::test]
    async fn apply_recovery_policy_leaves_stuck_when_attempts_remain() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.set_recovery(&job.id, RecoveryPolicy::AutoResume { max_attempts: 3 })
            .await
            .unwrap();
        mgr.start(&job.id).await.unwrap();
        mgr.stuck(&job.id, "hung").await.unwrap();

        let stats = mgr.apply_recovery_policy().await.unwrap();
        assert_eq!(stats.abandoned, 0);
        assert_eq!(stats.left_stuck, 1);
    }

    #[tokio::test]
    async fn apply_recovery_policy_on_empty_store_returns_zero() {
        let mgr = make_manager();
        let stats = mgr.apply_recovery_policy().await.unwrap();
        assert_eq!(stats.abandoned, 0);
        assert_eq!(stats.left_stuck, 0);
    }

    /// Drop the second-to-last transition record from a job's history.
    /// Simulates a stranded `Auto` job that was saved as `Submitted` in
    /// some earlier process before this PR's in-memory-atomic complete()
    /// landed — i.e. exactly the case `reconcile_auto_chains` exists for.
    async fn force_status(mgr: &JobManager, job_id: &str, status: JobStatus) {
        let mut job = mgr.store.get(job_id).await.unwrap().unwrap();
        job.status = status;
        mgr.store.save(&job).await.unwrap();
    }

    #[tokio::test]
    async fn reconcile_auto_chains_advances_completed_auto_to_accepted() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        force_status(&mgr, &job.id, JobStatus::Completed).await;

        let moved = mgr.reconcile_auto_chains().await.unwrap();
        assert_eq!(moved, 1);

        let final_job = mgr.store.get(&job.id).await.unwrap().unwrap();
        assert_eq!(final_job.status, JobStatus::Accepted);
        assert!(final_job.submitted_at.is_some());
        assert!(final_job.accepted_at.is_some());
    }

    #[tokio::test]
    async fn reconcile_auto_chains_advances_submitted_auto_to_accepted() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        force_status(&mgr, &job.id, JobStatus::Submitted).await;

        let moved = mgr.reconcile_auto_chains().await.unwrap();
        assert_eq!(moved, 1);

        let final_job = mgr.store.get(&job.id).await.unwrap().unwrap();
        assert_eq!(final_job.status, JobStatus::Accepted);
    }

    #[tokio::test]
    async fn reconcile_auto_chains_leaves_manual_completed_alone() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.set_acceptance(
            &job.id,
            AcceptancePolicy::Manual {
                submitter: aura_job::Acceptor::User,
                acceptor: aura_job::Acceptor::User,
            },
        )
        .await
        .unwrap();
        force_status(&mgr, &job.id, JobStatus::Completed).await;

        let moved = mgr.reconcile_auto_chains().await.unwrap();
        assert_eq!(moved, 0);

        let still = mgr.store.get(&job.id).await.unwrap().unwrap();
        assert_eq!(still.status, JobStatus::Completed);
    }

    #[tokio::test]
    async fn bootstrap_recovery_chains_steps_in_order() {
        let mgr = make_manager();

        // Stuck-with-Abandon job: should become Abandoned by step 3.
        let abandon_job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.set_recovery(&abandon_job.id, RecoveryPolicy::Abandon)
            .await
            .unwrap();
        mgr.start(&abandon_job.id).await.unwrap();
        mgr.stuck(&abandon_job.id, "hung").await.unwrap();

        // InProgress job from a "previous run": step 1 → Stuck;
        // step 3 with default AutoResume keeps it Stuck.
        let crashed_job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.start(&crashed_job.id).await.unwrap();

        // Auto job stranded at Submitted: step 2 → Accepted.
        let stranded_job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        force_status(&mgr, &stranded_job.id, JobStatus::Submitted).await;

        let stats = mgr.bootstrap_recovery().await.unwrap();
        assert_eq!(stats.interrupted, 1);
        assert_eq!(stats.reconciled, 1);
        assert_eq!(stats.recovery.abandoned, 1);
        assert_eq!(stats.recovery.left_stuck, 1);

        assert_eq!(
            mgr.store
                .get(&abandon_job.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            JobStatus::Abandoned
        );
        assert_eq!(
            mgr.store
                .get(&crashed_job.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            JobStatus::Stuck
        );
        assert_eq!(
            mgr.store
                .get(&stranded_job.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            JobStatus::Accepted
        );
    }

    #[tokio::test]
    async fn list_returns_all_jobs_when_status_is_none() {
        let mgr = make_manager();
        mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.create_job("s2", test_kind(), None).await.unwrap();

        let all = mgr.list(None).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn list_filters_by_status() {
        let mgr = make_manager();
        let pending = mgr.create_job("s1", test_kind(), None).await.unwrap();
        let running = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.start(&running.id).await.unwrap();

        let pendings = mgr.list(Some(JobStatus::Pending)).await.unwrap();
        assert_eq!(pendings.len(), 1);
        assert_eq!(pendings[0].id, pending.id);

        let runnings = mgr.list(Some(JobStatus::InProgress)).await.unwrap();
        assert_eq!(runnings.len(), 1);
        assert_eq!(runnings[0].id, running.id);
    }

    #[tokio::test]
    async fn get_returns_none_for_missing() {
        let mgr = make_manager();
        assert!(mgr.get("missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cancel_pending_job_goes_to_cancelled() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();

        let out = mgr.cancel(&job.id).await.unwrap();
        assert_eq!(out.status, JobStatus::Cancelled);

        let history = mgr.get_history(&job.id).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].from, JobStatus::Pending);
        assert_eq!(history[0].to, JobStatus::Cancelled);
        assert_eq!(history[0].reason.as_deref(), Some("cancelled by operator"));
    }

    #[tokio::test]
    async fn cancel_in_progress_job_goes_to_cancelled() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.start(&job.id).await.unwrap();

        let out = mgr.cancel(&job.id).await.unwrap();
        assert_eq!(out.status, JobStatus::Cancelled);
    }

    #[tokio::test]
    async fn cancel_stuck_job_goes_to_cancelled() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.start(&job.id).await.unwrap();
        mgr.stuck(&job.id, "hung").await.unwrap();

        let out = mgr.cancel(&job.id).await.unwrap();
        assert_eq!(out.status, JobStatus::Cancelled);
    }

    #[tokio::test]
    async fn cancel_terminal_job_errors() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.start(&job.id).await.unwrap();
        mgr.fail(&job.id, "nope").await.unwrap();

        let err = mgr.cancel(&job.id).await.unwrap_err();
        assert!(matches!(err, JobError::InvalidTransition(_)));
    }

    #[tokio::test]
    async fn cancel_completed_job_errors() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.start(&job.id).await.unwrap();
        mgr.complete(&job.id, serde_json::json!(null))
            .await
            .unwrap();

        let err = mgr.cancel(&job.id).await.unwrap_err();
        assert!(matches!(err, JobError::InvalidTransition(_)));
    }

    #[tokio::test]
    async fn cancel_missing_job_errors() {
        let mgr = make_manager();
        let err = mgr.cancel("nonexistent").await.unwrap_err();
        assert!(matches!(err, JobError::NotFound(_)));
    }

    #[tokio::test]
    async fn parent_child_relationship() {
        let mgr = make_manager();
        let parent = mgr.create_job("s1", test_kind(), None).await.unwrap();
        let child = mgr
            .create_job("s1", test_kind(), Some(&parent.id))
            .await
            .unwrap();
        assert_eq!(child.parent_job_id.as_deref(), Some(parent.id.as_str()));

        let children = mgr.store.list_children(&parent.id).await.unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, child.id);
    }
}
