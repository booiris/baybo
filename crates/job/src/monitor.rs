use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tracing::{info, warn};

use crate::manager::JobManager;
use crate::{JobStatus, JobStore};

/// Configuration for stuck job detection.
#[derive(Debug, Clone)]
pub struct StuckJobConfig {
    /// How long a job can be InProgress before being marked Stuck.
    pub stuck_threshold: Duration,
    /// How often to scan for stuck jobs.
    pub scan_interval: Duration,
    /// Maximum number of recovery attempts before failing a job.
    pub max_recovery_attempts: u32,
}

impl Default for StuckJobConfig {
    fn default() -> Self {
        Self {
            stuck_threshold: Duration::from_secs(300), // 5 minutes
            scan_interval: Duration::from_secs(60),    // 1 minute
            max_recovery_attempts: 3,
        }
    }
}

/// Monitors jobs for stuck states and performs automatic recovery.
pub struct StuckJobMonitor {
    job_manager: Arc<JobManager>,
    store: Arc<dyn JobStore>,
    config: StuckJobConfig,
}

impl StuckJobMonitor {
    pub fn new(
        job_manager: Arc<JobManager>,
        store: Arc<dyn JobStore>,
        config: StuckJobConfig,
    ) -> Self {
        Self {
            job_manager,
            store,
            config,
        }
    }

    /// Spawn a background task that periodically scans for stuck jobs.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(self.config.scan_interval);
            loop {
                ticker.tick().await;
                if let Err(e) = self.scan_once().await {
                    warn!(error = %e, "stuck job scan failed");
                }
            }
        })
    }

    /// Run a single scan: detect InProgress jobs past threshold, mark them Stuck,
    /// then check existing Stuck jobs and either recover or fail them.
    async fn scan_once(&self) -> aura_core::Result<()> {
        self.detect_stuck_jobs().await?;
        self.handle_stuck_jobs().await?;
        Ok(())
    }

    /// Find InProgress jobs that have exceeded the stuck threshold and mark them Stuck.
    async fn detect_stuck_jobs(&self) -> aura_core::Result<()> {
        let in_progress = self.store.list_by_status(JobStatus::InProgress).await?;
        let now = Utc::now();
        let threshold = chrono::Duration::from_std(self.config.stuck_threshold)
            .unwrap_or(chrono::Duration::seconds(300));

        for job in in_progress {
            let started = match job.started_at {
                Some(t) => t,
                None => job.created_at,
            };
            if now - started > threshold {
                info!(job_id = %job.id, session_id = %job.session_id, "marking job as stuck");
                if let Err(e) = self
                    .job_manager
                    .stuck(&job.id, "exceeded stuck threshold")
                    .await
                {
                    warn!(job_id = %job.id, error = %e, "failed to mark job as stuck");
                }
            }
        }
        Ok(())
    }

    /// Check existing Stuck jobs: count recovery attempts, either recover or fail.
    async fn handle_stuck_jobs(&self) -> aura_core::Result<()> {
        let stuck_jobs = self.store.list_by_status(JobStatus::Stuck).await?;

        for job in stuck_jobs {
            let transitions = self.store.get_transitions(&job.id).await?;
            // Count how many times this job has been recovered (Stuck -> InProgress).
            let recovery_count = transitions
                .iter()
                .filter(|t| t.from == JobStatus::Stuck && t.to == JobStatus::InProgress)
                .count() as u32;

            if recovery_count >= self.config.max_recovery_attempts {
                info!(
                    job_id = %job.id,
                    recovery_count,
                    "max recovery attempts reached, failing job"
                );
                if let Err(e) = self
                    .job_manager
                    .fail(
                        &job.id,
                        &format!("exceeded max recovery attempts ({recovery_count})"),
                    )
                    .await
                {
                    warn!(job_id = %job.id, error = %e, "failed to fail stuck job");
                }
            } else {
                info!(
                    job_id = %job.id,
                    recovery_count,
                    max = self.config.max_recovery_attempts,
                    "recovering stuck job"
                );
                if let Err(e) = self
                    .job_manager
                    .recover(
                        &job.id,
                        &format!("auto-recovery attempt {}", recovery_count + 1),
                    )
                    .await
                {
                    warn!(job_id = %job.id, error = %e, "failed to recover stuck job");
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Job, JobTransition};
    use async_trait::async_trait;
    use aura_core::{AuraError, OperationKind};
    use serde_json::Value;
    use std::sync::Mutex;

    /// Simple in-memory store for testing (mirrors manager.rs tests).
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

    fn lock_err<T>(_: T) -> AuraError {
        AuraError::Internal(anyhow::anyhow!("lock poisoned"))
    }

    #[async_trait]
    impl JobStore for InMemoryJobStore {
        async fn create(&self, job: &Job) -> aura_core::Result<()> {
            self.jobs.lock().map_err(lock_err)?.push(job.clone());
            Ok(())
        }

        async fn get(&self, job_id: &str) -> aura_core::Result<Option<Job>> {
            let jobs = self.jobs.lock().map_err(lock_err)?;
            Ok(jobs.iter().find(|j| j.id == job_id).cloned())
        }

        async fn update_status(
            &self,
            job_id: &str,
            status: JobStatus,
            output: Option<Value>,
            error: Option<String>,
        ) -> aura_core::Result<()> {
            let mut jobs = self.jobs.lock().map_err(lock_err)?;
            let job = jobs
                .iter_mut()
                .find(|j| j.id == job_id)
                .ok_or_else(|| AuraError::NotFound(format!("job {job_id}")))?;

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

        async fn list_by_session(&self, session_id: &str) -> aura_core::Result<Vec<Job>> {
            let jobs = self.jobs.lock().map_err(lock_err)?;
            Ok(jobs
                .iter()
                .filter(|j| j.session_id == session_id)
                .cloned()
                .collect())
        }

        async fn list_by_status(&self, status: JobStatus) -> aura_core::Result<Vec<Job>> {
            let jobs = self.jobs.lock().map_err(lock_err)?;
            Ok(jobs
                .iter()
                .filter(|j| j.status == status)
                .cloned()
                .collect())
        }

        async fn list_children(&self, parent_job_id: &str) -> aura_core::Result<Vec<Job>> {
            let jobs = self.jobs.lock().map_err(lock_err)?;
            Ok(jobs
                .iter()
                .filter(|j| j.parent_job_id.as_deref() == Some(parent_job_id))
                .cloned()
                .collect())
        }

        async fn record_transition(&self, transition: &JobTransition) -> aura_core::Result<()> {
            self.transitions
                .lock()
                .map_err(lock_err)?
                .push(transition.clone());
            Ok(())
        }

        async fn get_transitions(&self, job_id: &str) -> aura_core::Result<Vec<JobTransition>> {
            let transitions = self.transitions.lock().map_err(lock_err)?;
            Ok(transitions
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

    /// Helper: create a shared store, manager, and monitor for testing.
    fn make_test_setup(
        config: StuckJobConfig,
    ) -> (Arc<InMemoryJobStore>, Arc<JobManager>, StuckJobMonitor) {
        let store = Arc::new(InMemoryJobStore::new());
        let manager = Arc::new(JobManager::new(Box::new(ArcJobStore(store.clone()))));
        let monitor = StuckJobMonitor::new(manager.clone(), store.clone(), config);
        (store, manager, monitor)
    }

    /// Wrapper to use Arc<InMemoryJobStore> as Box<dyn JobStore>.
    struct ArcJobStore(Arc<InMemoryJobStore>);

    #[async_trait]
    impl JobStore for ArcJobStore {
        async fn create(&self, job: &Job) -> aura_core::Result<()> {
            self.0.create(job).await
        }
        async fn get(&self, job_id: &str) -> aura_core::Result<Option<Job>> {
            self.0.get(job_id).await
        }
        async fn update_status(
            &self,
            job_id: &str,
            status: JobStatus,
            output: Option<Value>,
            error: Option<String>,
        ) -> aura_core::Result<()> {
            self.0.update_status(job_id, status, output, error).await
        }
        async fn list_by_session(&self, session_id: &str) -> aura_core::Result<Vec<Job>> {
            self.0.list_by_session(session_id).await
        }
        async fn list_by_status(&self, status: JobStatus) -> aura_core::Result<Vec<Job>> {
            self.0.list_by_status(status).await
        }
        async fn list_children(&self, parent_job_id: &str) -> aura_core::Result<Vec<Job>> {
            self.0.list_children(parent_job_id).await
        }
        async fn record_transition(&self, transition: &JobTransition) -> aura_core::Result<()> {
            self.0.record_transition(transition).await
        }
        async fn get_transitions(&self, job_id: &str) -> aura_core::Result<Vec<JobTransition>> {
            self.0.get_transitions(job_id).await
        }
    }

    /// Helper to create a job in InProgress state with an old started_at timestamp.
    async fn create_old_in_progress_job(
        store: &InMemoryJobStore,
        manager: &JobManager,
        age: chrono::Duration,
    ) -> Job {
        let job = manager.create_job("s1", test_kind(), None).await.unwrap();
        manager.start(&job.id).await.unwrap();

        // Backdate started_at so the job appears old.
        {
            let mut jobs = store.jobs.lock().unwrap();
            let j = jobs.iter_mut().find(|j| j.id == job.id).unwrap();
            j.started_at = Some(Utc::now() - age);
        }

        store.get(&job.id).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn detect_stuck_jobs_marks_old_jobs() {
        let config = StuckJobConfig {
            stuck_threshold: Duration::from_secs(60),
            scan_interval: Duration::from_secs(10),
            max_recovery_attempts: 3,
        };
        let (store, manager, monitor) = make_test_setup(config);

        // Create an old job that exceeds the threshold.
        let old_job =
            create_old_in_progress_job(&store, &manager, chrono::Duration::seconds(120)).await;

        // Create a fresh job that should not be marked stuck.
        let fresh_job = manager.create_job("s1", test_kind(), None).await.unwrap();
        manager.start(&fresh_job.id).await.unwrap();

        monitor.detect_stuck_jobs().await.unwrap();

        let old = store.get(&old_job.id).await.unwrap().unwrap();
        assert_eq!(old.status, JobStatus::Stuck);

        let fresh = store.get(&fresh_job.id).await.unwrap().unwrap();
        assert_eq!(fresh.status, JobStatus::InProgress);
    }

    #[tokio::test]
    async fn handle_stuck_jobs_recovers_within_limit() {
        let config = StuckJobConfig {
            stuck_threshold: Duration::from_secs(60),
            scan_interval: Duration::from_secs(10),
            max_recovery_attempts: 3,
        };
        let (store, manager, monitor) = make_test_setup(config);

        let job = manager.create_job("s1", test_kind(), None).await.unwrap();
        manager.start(&job.id).await.unwrap();
        manager.stuck(&job.id, "test stuck").await.unwrap();

        // No previous recovery attempts, so it should recover.
        monitor.handle_stuck_jobs().await.unwrap();

        let updated = store.get(&job.id).await.unwrap().unwrap();
        assert_eq!(updated.status, JobStatus::InProgress);
    }

    #[tokio::test]
    async fn handle_stuck_jobs_fails_after_max_attempts() {
        let config = StuckJobConfig {
            stuck_threshold: Duration::from_secs(60),
            scan_interval: Duration::from_secs(10),
            max_recovery_attempts: 2,
        };
        let (store, manager, monitor) = make_test_setup(config);

        let job = manager.create_job("s1", test_kind(), None).await.unwrap();
        manager.start(&job.id).await.unwrap();

        // Simulate 2 recovery cycles (stuck -> recover -> stuck -> recover -> stuck).
        for _ in 0..2 {
            manager.stuck(&job.id, "stuck again").await.unwrap();
            manager.recover(&job.id, "manual recovery").await.unwrap();
        }

        // Now mark stuck a third time; recovery count == 2 == max_recovery_attempts.
        manager.stuck(&job.id, "stuck final").await.unwrap();

        monitor.handle_stuck_jobs().await.unwrap();

        let updated = store.get(&job.id).await.unwrap().unwrap();
        assert_eq!(updated.status, JobStatus::Failed);
    }

    #[tokio::test]
    async fn scan_once_end_to_end() {
        let config = StuckJobConfig {
            stuck_threshold: Duration::from_secs(30),
            scan_interval: Duration::from_secs(10),
            max_recovery_attempts: 1,
        };
        let (store, manager, monitor) = make_test_setup(config);

        // Create a job that is old enough to be detected as stuck.
        let job = create_old_in_progress_job(&store, &manager, chrono::Duration::seconds(60)).await;

        // First scan: detect_stuck_jobs marks it Stuck, then handle_stuck_jobs recovers it.
        monitor.scan_once().await.unwrap();
        let after_first = store.get(&job.id).await.unwrap().unwrap();
        assert_eq!(after_first.status, JobStatus::InProgress);

        // Backdate again so it gets detected as stuck on the next scan.
        {
            let mut jobs = store.jobs.lock().unwrap();
            let j = jobs.iter_mut().find(|j| j.id == job.id).unwrap();
            j.started_at = Some(Utc::now() - chrono::Duration::seconds(60));
        }

        // Second scan: detected stuck again, but now recovery_count == 1 == max, so it fails.
        monitor.scan_once().await.unwrap();
        let after_second = store.get(&job.id).await.unwrap().unwrap();
        assert_eq!(after_second.status, JobStatus::Failed);
    }
}
