use std::sync::Arc;

use aura_job::{Job, JobError, JobStatus, JobTransition, OperationKind};
use aura_storage::JobStore;

type Result<T> = std::result::Result<T, JobError>;

/// Manages job lifecycle: load from store, apply transition, persist.
///
/// State machine validation and timestamp management live on `Job` itself.
/// This manager is a thin persistence orchestrator.
pub struct JobManager {
    store: Arc<dyn JobStore>,
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

    pub async fn complete(&self, job_id: &str, output: serde_json::Value) -> Result<()> {
        let (job, transition) = self.apply(job_id, |j| j.complete(output)).await?;
        self.persist(job, transition).await
    }

    pub async fn submit(&self, job_id: &str) -> Result<()> {
        let (job, transition) = self.apply(job_id, |j| j.submit()).await?;
        self.persist(job, transition).await
    }

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

    /// Cancel a job. Transitions the job to `Failed` with a cancellation
    /// reason. `Pending` jobs are first started, then failed (the state
    /// machine does not allow `Pending -> Failed` directly). Terminal jobs
    /// (`Accepted`, `Failed`) and already-settled jobs (`Completed`,
    /// `Submitted`) are rejected as no-ops so the operator gets a clear
    /// error rather than silently losing audit trail for work that has
    /// already produced output.
    pub async fn cancel(&self, job_id: &str) -> Result<Job> {
        let job = self.load_job(job_id).await?;
        let reason = "cancelled by operator";
        match job.status {
            JobStatus::Pending => {
                self.start(job_id).await?;
                self.fail(job_id, reason).await?;
            }
            JobStatus::InProgress | JobStatus::Stuck => {
                self.fail(job_id, reason).await?;
            }
            other => {
                return Err(JobError::InvalidTransition(format!(
                    "cannot cancel job {job_id} in status {other}"
                )));
            }
        }
        self.load_job(job_id).await
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
    async fn full_success_path() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        assert_eq!(job.status, JobStatus::Pending);

        mgr.start(&job.id).await.unwrap();
        mgr.complete(&job.id, serde_json::json!({"ok": true}))
            .await
            .unwrap();
        mgr.submit(&job.id).await.unwrap();
        mgr.accept(&job.id).await.unwrap();

        let history = mgr.get_history(&job.id).await.unwrap();
        assert_eq!(history.len(), 4);
        assert_eq!(history[0].from, JobStatus::Pending);
        assert_eq!(history[0].to, JobStatus::InProgress);
        assert_eq!(history[3].to, JobStatus::Accepted);
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
        mgr.submit(&job.id).await.unwrap();
        mgr.accept(&job.id).await.unwrap();

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

        // Create jobs in various states
        let pending_job = mgr.create_job("s1", test_kind(), None).await.unwrap();

        let in_progress_job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.start(&in_progress_job.id).await.unwrap();

        let completed_job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.start(&completed_job.id).await.unwrap();
        mgr.complete(&completed_job.id, serde_json::json!(null))
            .await
            .unwrap();

        let accepted_job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.start(&accepted_job.id).await.unwrap();
        mgr.complete(&accepted_job.id, serde_json::json!(null))
            .await
            .unwrap();
        mgr.submit(&accepted_job.id).await.unwrap();
        mgr.accept(&accepted_job.id).await.unwrap();

        let failed_job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.start(&failed_job.id).await.unwrap();
        mgr.fail(&failed_job.id, "err").await.unwrap();

        // Recover — only the InProgress job should transition
        let count = mgr.recover_interrupted().await.unwrap();
        assert_eq!(count, 1);

        // Verify the in_progress job is now Stuck
        let job = mgr.store.get(&in_progress_job.id).await.unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Stuck);

        // Verify others unchanged
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
    async fn cancel_pending_job_transitions_through_in_progress() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();

        let out = mgr.cancel(&job.id).await.unwrap();
        assert_eq!(out.status, JobStatus::Failed);
        assert_eq!(out.error.as_deref(), Some("cancelled by operator"));

        let history = mgr.get_history(&job.id).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].to, JobStatus::InProgress);
        assert_eq!(history[1].to, JobStatus::Failed);
    }

    #[tokio::test]
    async fn cancel_in_progress_job_fails_directly() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.start(&job.id).await.unwrap();

        let out = mgr.cancel(&job.id).await.unwrap();
        assert_eq!(out.status, JobStatus::Failed);
    }

    #[tokio::test]
    async fn cancel_stuck_job_fails() {
        let mgr = make_manager();
        let job = mgr.create_job("s1", test_kind(), None).await.unwrap();
        mgr.start(&job.id).await.unwrap();
        mgr.stuck(&job.id, "hung").await.unwrap();

        let out = mgr.cancel(&job.id).await.unwrap();
        assert_eq!(out.status, JobStatus::Failed);
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
