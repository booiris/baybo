use chrono::Utc;
use serde_json::Value;
use uuid::Uuid;

use aura_job::{Job, JobError, JobStatus, JobTransition, OperationKind};
use aura_storage::JobStore;

type Result<T> = std::result::Result<T, JobError>;

/// Manages job lifecycle with strict state-machine validation.
pub struct JobManager {
    store: Box<dyn JobStore>,
}

impl JobManager {
    pub fn new(store: Box<dyn JobStore>) -> Self {
        Self { store }
    }

    pub async fn create_job(
        &self,
        session_id: &str,
        kind: OperationKind,
        parent: Option<&str>,
    ) -> Result<Job> {
        let job = Job {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.to_owned(),
            parent_job_id: parent.map(String::from),
            kind,
            status: JobStatus::Pending,
            input: None,
            output: None,
            error: None,
            trace_span_id: None,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
        };
        self.store.create(&job).await?;
        Ok(job)
    }

    pub async fn start(&self, job_id: &str) -> Result<()> {
        self.transition(job_id, JobStatus::InProgress, None, None, None)
            .await
    }

    pub async fn complete(&self, job_id: &str, output: Value) -> Result<()> {
        self.transition(job_id, JobStatus::Completed, Some(output), None, None)
            .await
    }

    pub async fn submit(&self, job_id: &str) -> Result<()> {
        self.transition(job_id, JobStatus::Submitted, None, None, None)
            .await
    }

    pub async fn accept(&self, job_id: &str) -> Result<()> {
        self.transition(job_id, JobStatus::Accepted, None, None, None)
            .await
    }

    pub async fn fail(&self, job_id: &str, error: &str) -> Result<()> {
        self.transition(
            job_id,
            JobStatus::Failed,
            None,
            Some(error.to_owned()),
            None,
        )
        .await
    }

    async fn load_job(&self, job_id: &str) -> Result<Job> {
        self.store
            .get(job_id)
            .await?
            .ok_or_else(|| JobError::NotFound(format!("job {job_id}")))
    }

    async fn transition(
        &self,
        job_id: &str,
        target: JobStatus,
        output: Option<Value>,
        error: Option<String>,
        reason: Option<String>,
    ) -> Result<()> {
        let job = self.load_job(job_id).await?;

        if !job.status.can_transition_to(&target) {
            return Err(JobError::InvalidTransition(format!(
                "{} -> {} (job {})",
                job.status, target, job_id
            )));
        }

        self.store
            .update_status(job_id, target.clone(), output, error)
            .await?;

        let transition = JobTransition {
            job_id: job_id.to_owned(),
            from: job.status,
            to: target,
            reason,
            timestamp: Utc::now(),
        };
        self.store.record_transition(&transition).await?;

        Ok(())
    }
}

#[cfg(test)]
impl JobManager {
    async fn stuck(&self, job_id: &str, reason: &str) -> Result<()> {
        self.transition(
            job_id,
            JobStatus::Stuck,
            None,
            None,
            Some(reason.to_owned()),
        )
        .await
    }

    async fn recover(&self, job_id: &str, reason: &str) -> Result<()> {
        self.transition(
            job_id,
            JobStatus::InProgress,
            None,
            None,
            Some(reason.to_owned()),
        )
        .await
    }

    async fn get_history(&self, job_id: &str) -> Result<Vec<JobTransition>> {
        self.store.get_transitions(job_id).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use async_trait::async_trait;

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
            self.jobs.lock().map_err(lock_err)?.push(job.clone());
            Ok(())
        }

        async fn get(&self, job_id: &str) -> Result<Option<Job>> {
            let jobs = self.jobs.lock().map_err(lock_err)?;
            Ok(jobs.iter().find(|j| j.id == job_id).cloned())
        }

        async fn update_status(
            &self,
            job_id: &str,
            status: JobStatus,
            output: Option<Value>,
            error: Option<String>,
        ) -> Result<()> {
            let mut jobs = self.jobs.lock().map_err(lock_err)?;
            let job = jobs
                .iter_mut()
                .find(|j| j.id == job_id)
                .ok_or_else(|| JobError::NotFound(format!("job {job_id}")))?;

            let now = Utc::now();
            if status == JobStatus::InProgress && job.started_at.is_none() {
                job.started_at = Some(now);
            }
            if matches!(
                status,
                JobStatus::Completed | JobStatus::Accepted | JobStatus::Failed
            ) {
                job.completed_at = Some(now);
            }

            job.status = status;
            if let Some(o) = output {
                job.output = Some(o);
            }
            if let Some(e) = error {
                job.error = Some(e);
            }
            Ok(())
        }

        async fn list_by_session(&self, session_id: &str) -> Result<Vec<Job>> {
            let jobs = self.jobs.lock().map_err(lock_err)?;
            Ok(jobs
                .iter()
                .filter(|j| j.session_id == session_id)
                .cloned()
                .collect())
        }

        async fn list_by_status(&self, status: JobStatus) -> Result<Vec<Job>> {
            let jobs = self.jobs.lock().map_err(lock_err)?;
            Ok(jobs
                .iter()
                .filter(|j| j.status == status)
                .cloned()
                .collect())
        }

        async fn list_children(&self, parent_job_id: &str) -> Result<Vec<Job>> {
            let jobs = self.jobs.lock().map_err(lock_err)?;
            Ok(jobs
                .iter()
                .filter(|j| j.parent_job_id.as_deref() == Some(parent_job_id))
                .cloned()
                .collect())
        }

        async fn record_transition(&self, transition: &JobTransition) -> Result<()> {
            self.transitions
                .lock()
                .map_err(lock_err)?
                .push(transition.clone());
            Ok(())
        }

        async fn get_transitions(&self, job_id: &str) -> Result<Vec<JobTransition>> {
            let transitions = self.transitions.lock().map_err(lock_err)?;
            Ok(transitions
                .iter()
                .filter(|t| t.job_id == job_id)
                .cloned()
                .collect())
        }
    }

    fn lock_err<T>(_: T) -> JobError {
        JobError::Internal(anyhow::anyhow!("lock poisoned"))
    }

    fn test_kind() -> OperationKind {
        OperationKind::LlmCall {
            model: "test-model".into(),
        }
    }

    fn make_manager() -> JobManager {
        JobManager::new(Box::new(InMemoryJobStore::new()))
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
