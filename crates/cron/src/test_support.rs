//! Test-only fixtures for the cron domain. Gated behind `test-support` so
//! nothing here ships in a release build; downstream crates pull it in with
//! `baybo-cron = { workspace = true, features = ["test-support"] }` in their
//! `[dev-dependencies]`.

use async_trait::async_trait;
use baybo_model::{CronExecution, CronJob, CronStatus, ExecutionStatus};
use baybo_store::StorageError;
use baybo_store::cron::{CronStore, ExecutionCompletion, Result};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;

/// In-memory [`CronStore`] — the whole cron domain against `Vec`s, so a test
/// can drive the scheduler (and the agent layer's cron waiter / boot re-drive)
/// without libsql.
#[derive(Default)]
pub struct InMemoryCronStore {
    jobs: Mutex<Vec<CronJob>>,
    executions: Mutex<Vec<CronExecution>>,
}

impl InMemoryCronStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every execution row, for assertions on the delivery ledger.
    pub fn executions(&self) -> Vec<CronExecution> {
        self.executions.lock().clone()
    }

    /// One execution row by id.
    pub fn execution(&self, execution_id: &str) -> Option<CronExecution> {
        self.executions
            .lock()
            .iter()
            .find(|e| e.id == execution_id)
            .cloned()
    }

    /// Mutate an execution in place, or return `NotFound`. The ledger writers
    /// are read-modify-write on the stored row, exactly like the libsql impl.
    fn with_execution<F>(&self, execution_id: &str, f: F) -> Result<()>
    where
        F: FnOnce(&mut CronExecution),
    {
        let mut execs = self.executions.lock();
        match execs.iter_mut().find(|e| e.id == execution_id) {
            Some(exec) => {
                f(exec);
                Ok(())
            }
            None => Err(StorageError::NotFound(execution_id.to_string())),
        }
    }
}

#[async_trait]
impl CronStore for InMemoryCronStore {
    async fn create(&self, job: &CronJob) -> Result<()> {
        self.jobs.lock().push(job.clone());
        Ok(())
    }

    async fn get(&self, job_id: &str) -> Result<Option<CronJob>> {
        Ok(self.jobs.lock().iter().find(|j| j.id == job_id).cloned())
    }

    async fn save(&self, job: &CronJob) -> Result<()> {
        let mut jobs = self.jobs.lock();
        if let Some(existing) = jobs.iter_mut().find(|j| j.id == job.id) {
            *existing = job.clone();
            Ok(())
        } else {
            Err(StorageError::NotFound(job.id.clone()))
        }
    }

    async fn delete(&self, job_id: &str) -> Result<()> {
        let mut jobs = self.jobs.lock();
        let len_before = jobs.len();
        jobs.retain(|j| j.id != job_id);
        if jobs.len() == len_before {
            Err(StorageError::NotFound(job_id.to_string()))
        } else {
            Ok(())
        }
    }

    async fn list_by_user(&self, user_id: &str) -> Result<Vec<CronJob>> {
        Ok(self
            .jobs
            .lock()
            .iter()
            .filter(|j| j.user_id == user_id)
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> Result<Vec<CronJob>> {
        Ok(self.jobs.lock().clone())
    }

    async fn list_enabled(&self) -> Result<Vec<CronJob>> {
        Ok(self
            .jobs
            .lock()
            .iter()
            .filter(|j| j.status == CronStatus::Enabled)
            .cloned()
            .collect())
    }

    async fn list_due(&self, now_us: i64) -> Result<Vec<CronJob>> {
        Ok(self
            .jobs
            .lock()
            .iter()
            .filter(|j| {
                j.status == CronStatus::Enabled
                    && j.next_trigger_at
                        .is_some_and(|t| t.timestamp_micros() <= now_us)
            })
            .cloned()
            .collect())
    }

    async fn record_execution(&self, exec: &CronExecution) -> Result<()> {
        self.executions.lock().push(exec.clone());
        Ok(())
    }

    async fn list_executions_by_job(&self, job_id: &str) -> Result<Vec<CronExecution>> {
        Ok(self
            .executions
            .lock()
            .iter()
            .filter(|e| e.job_id == job_id)
            .cloned()
            .collect())
    }

    async fn list_executions_by_user(&self, user_id: &str) -> Result<Vec<CronExecution>> {
        Ok(self
            .executions
            .lock()
            .iter()
            .filter(|e| e.user_id == user_id)
            .cloned()
            .collect())
    }

    async fn has_execution_for_schedule(
        &self,
        job_id: &str,
        scheduled_fire_time_us: i64,
    ) -> Result<bool> {
        Ok(self.executions.lock().iter().any(|e| {
            e.job_id == job_id && e.scheduled_fire_time.timestamp_micros() == scheduled_fire_time_us
        }))
    }

    async fn update_execution_status(
        &self,
        execution_id: &str,
        status: ExecutionStatus,
    ) -> Result<()> {
        self.with_execution(execution_id, |exec| exec.status = status)
    }

    async fn list_executions_by_status(
        &self,
        status: ExecutionStatus,
    ) -> Result<Vec<CronExecution>> {
        Ok(self
            .executions
            .lock()
            .iter()
            .filter(|e| e.status == status)
            .cloned()
            .collect())
    }

    async fn record_execution_completion(
        &self,
        execution_id: &str,
        completion: ExecutionCompletion,
    ) -> Result<()> {
        self.with_execution(execution_id, |exec| {
            exec.fire_session_id = Some(completion.fire_session_id);
            exec.outcome = Some(completion.outcome);
            exec.reply_ordinal = completion.reply_ordinal;
            exec.completed_at = Some(completion.completed_at);
        })
    }

    async fn mark_execution_notified(&self, execution_id: &str, at: DateTime<Utc>) -> Result<()> {
        self.with_execution(execution_id, |exec| exec.notified_at = Some(at))
    }

    async fn list_executions_awaiting_delivery(&self) -> Result<Vec<CronExecution>> {
        Ok(self
            .executions
            .lock()
            .iter()
            .filter(|e| e.awaits_delivery())
            .cloned()
            .collect())
    }
}
