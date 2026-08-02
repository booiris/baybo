//! Test-only fixtures for the cron domain. Gated behind `test-support` so
//! nothing here ships in a release build; downstream crates pull it in with
//! `baybo-cron = { workspace = true, features = ["test-support"] }` in their
//! `[dev-dependencies]`.

use async_trait::async_trait;
use baybo_model::{CronExecution, CronJob, CronStatus, ExecutionStatus};
use baybo_store::StorageError;
use baybo_store::cron::{CronFire, CronStore, ExecutionCompletion, Result};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;

/// The sqlite `UNMOVED` predicate, against a `Vec`: the stored row is still
/// live and still carries the status and slot of the snapshot the caller read.
fn unmoved(stored: &CronJob, expected: &CronJob) -> bool {
    !stored.is_deleted()
        && stored.status == expected.status
        && stored.next_trigger_at == expected.next_trigger_at
}

/// In-memory [`CronStore`] — the whole cron domain against `Vec`s, so a test
/// can drive the scheduler (and the agent layer's cron waiter / boot re-drive)
/// without sqlite.
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
    /// are read-modify-write on the stored row, exactly like the sqlite impl.
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

    /// Write a job's recycle-bin state, mirroring the sqlite impl: the row is
    /// kept either way, only the `deleted_at` stamp moves.
    fn stamp_deleted_at(&self, job_id: &str, deleted_at: Option<DateTime<Utc>>) -> Result<()> {
        let mut jobs = self.jobs.lock();
        match jobs.iter_mut().find(|j| j.id == job_id) {
            Some(job) => {
                job.deleted_at = deleted_at;
                job.updated_at = Utc::now();
                Ok(())
            }
            None => Err(StorageError::NotFound(job_id.to_string())),
        }
    }

    fn is_deleted(&self, job_id: &str) -> Result<bool> {
        self.jobs
            .lock()
            .iter()
            .find(|j| j.id == job_id)
            .map(CronJob::is_deleted)
            .ok_or_else(|| StorageError::NotFound(job_id.to_string()))
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

    /// Mirrors the sqlite impl: a save carries everything about a job *except*
    /// its recycle-bin state, which only `delete` and `restore` may move. A
    /// caller writing back a snapshot it read before a deletion must not
    /// resurrect the job.
    async fn save(&self, job: &CronJob) -> Result<()> {
        let mut jobs = self.jobs.lock();
        if let Some(existing) = jobs.iter_mut().find(|j| j.id == job.id) {
            // Mirror the sqlite store: `deleted_at` and `pinned` are flat
            // columns `save` never writes, so a stale snapshot cannot clobber
            // them — without this the fake would let a scheduler tick silently
            // unpin a group (or resurrect a deleted job) and every test against
            // it would disagree with production.
            let deleted_at = existing.deleted_at;
            let pinned = existing.pinned;
            *existing = job.clone();
            existing.deleted_at = deleted_at;
            existing.pinned = pinned;
            Ok(())
        } else {
            Err(StorageError::NotFound(job.id.clone()))
        }
    }

    async fn set_pinned(&self, job_id: &str, pinned: bool) -> Result<bool> {
        let mut jobs = self.jobs.lock();
        match jobs.iter_mut().find(|j| j.id == job_id) {
            Some(job) => {
                job.pinned = pinned;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Mirrors the sqlite impl's conditional UPDATE: the write lands only while
    /// the row is still the live job the caller read, unmoved by a fire, a
    /// pause, or another edit — `updated_at` included, since an edit that
    /// rewrites only authored fields moves neither of the projected columns.
    async fn save_if_unchanged(&self, job: &CronJob, expected: &CronJob) -> Result<bool> {
        let mut jobs = self.jobs.lock();
        match jobs.iter_mut().find(|j| j.id == job.id) {
            Some(existing)
                if unmoved(existing, expected) && existing.updated_at == expected.updated_at =>
            {
                let deleted_at = existing.deleted_at;
                let pinned = existing.pinned;
                *existing = job.clone();
                existing.deleted_at = deleted_at;
                existing.pinned = pinned;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Mirrors the sqlite impl: the fire's fields are stamped onto the *stored*
    /// row, so an edit that landed while the slot was firing keeps the prompt,
    /// title and schedule the user gave it — a write-back carrying the whole
    /// pre-fire snapshot would revert them.
    async fn record_fire(&self, expected: &CronJob, fire: CronFire) -> Result<bool> {
        let mut jobs = self.jobs.lock();
        match jobs.iter_mut().find(|j| j.id == expected.id) {
            Some(existing) if unmoved(existing, expected) => {
                existing.status = fire.status;
                existing.next_trigger_at = fire.next_trigger_at;
                existing.last_triggered_at = Some(fire.last_triggered_at);
                existing.updated_at = fire.updated_at;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn delete(&self, job_id: &str) -> Result<()> {
        if self.is_deleted(job_id)? {
            return Ok(());
        }
        self.stamp_deleted_at(job_id, Some(Utc::now()))
    }

    async fn restore(&self, job_id: &str) -> Result<()> {
        self.stamp_deleted_at(job_id, None)
    }

    async fn list_by_user(&self, user_id: &str) -> Result<Vec<CronJob>> {
        Ok(self
            .jobs
            .lock()
            .iter()
            .filter(|j| !j.is_deleted() && j.user_id == user_id)
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> Result<Vec<CronJob>> {
        Ok(self
            .jobs
            .lock()
            .iter()
            .filter(|j| !j.is_deleted())
            .cloned()
            .collect())
    }

    async fn list_enabled(&self) -> Result<Vec<CronJob>> {
        Ok(self
            .jobs
            .lock()
            .iter()
            .filter(|j| !j.is_deleted() && j.status == CronStatus::Enabled)
            .cloned()
            .collect())
    }

    async fn list_deleted(&self) -> Result<Vec<CronJob>> {
        let mut deleted: Vec<CronJob> = self
            .jobs
            .lock()
            .iter()
            .filter(|j| j.is_deleted())
            .cloned()
            .collect();
        deleted.sort_by_key(|j| std::cmp::Reverse(j.deleted_at));
        Ok(deleted)
    }

    async fn list_due(&self, now_us: i64) -> Result<Vec<CronJob>> {
        Ok(self
            .jobs
            .lock()
            .iter()
            .filter(|j| {
                !j.is_deleted()
                    && j.status == CronStatus::Enabled
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
