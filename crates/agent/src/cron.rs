use std::str::FromStr;
use std::time::Duration;

use aura_cron::{CronError, CronExecution, CronJob, CronRunMode, CronStatus, ExecutionStatus};
use aura_session::ChannelType;
use aura_storage::{CronExecutionRow, CronJobRow, CronStore, CronStoreError};
use chrono::{DateTime, Utc};
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::service::ShutdownSignal;

// ── Error bridging ────────────────────────────────────────────────────

fn store_err(e: CronStoreError) -> CronError {
    match e {
        CronStoreError::NotFound(id) => CronError::NotFound(id),
        CronStoreError::Internal(msg) => CronError::Storage(msg),
    }
}

type Result<T> = std::result::Result<T, CronError>;

// ── Row ↔ Domain conversion ───────────────────────────────────────────

fn job_to_row(job: &CronJob) -> Result<CronJobRow> {
    let data = serde_json::to_string(job)
        .map_err(|e| CronError::Storage(format!("failed to serialize cron job: {e}")))?;
    let status_str = match job.status {
        CronStatus::Enabled => "enabled",
        CronStatus::Disabled => "disabled",
    };
    Ok(CronJobRow {
        id: job.id.clone(),
        user_id: job.user_id.clone(),
        status: status_str.to_string(),
        next_trigger_at: job
            .next_trigger_at
            .map(|t| t.to_rfc3339())
            .unwrap_or_default(),
        data,
    })
}

fn row_to_job(row: CronJobRow) -> Result<CronJob> {
    serde_json::from_str(&row.data)
        .map_err(|e| CronError::Storage(format!("failed to deserialize cron job: {e}")))
}

fn execution_to_row(exec: &CronExecution) -> Result<CronExecutionRow> {
    let data = serde_json::to_string(exec)
        .map_err(|e| CronError::Storage(format!("failed to serialize execution: {e}")))?;
    let status_str = match exec.status {
        ExecutionStatus::Pending => "pending",
        ExecutionStatus::Dispatched => "dispatched",
    };
    Ok(CronExecutionRow {
        id: exec.id.clone(),
        job_id: exec.job_id.clone(),
        user_id: exec.user_id.clone(),
        scheduled_fire_time: exec.scheduled_fire_time.to_rfc3339(),
        triggered_at: exec.triggered_at.to_rfc3339(),
        status: status_str.to_string(),
        data,
    })
}

fn row_to_execution(row: CronExecutionRow) -> Result<CronExecution> {
    let mut exec: CronExecution = serde_json::from_str(&row.data)
        .map_err(|e| CronError::Storage(format!("failed to deserialize execution: {e}")))?;
    // The row-level `status` column is the source of truth (updated independently of `data`).
    exec.status = match row.status.as_str() {
        "dispatched" => ExecutionStatus::Dispatched,
        _ => ExecutionStatus::Pending,
    };
    Ok(exec)
}

// ── Scheduler ──────────────────────────────────────────────────────────

/// Event emitted when a cron job fires.
#[derive(Debug, Clone)]
pub struct CronTriggerEvent {
    pub job_id: String,
    pub user_id: String,
    pub channel: ChannelType,
    pub prompt: String,
}

/// Manages cron job lifecycle and runs a background tick loop
/// that fires due jobs on schedule.
pub struct CronScheduler {
    store: Box<dyn CronStore>,
    trigger_tx: mpsc::Sender<CronTriggerEvent>,
    shutdown: ShutdownSignal,
}

impl CronScheduler {
    pub fn new(
        store: Box<dyn CronStore>,
        trigger_tx: mpsc::Sender<CronTriggerEvent>,
        shutdown: ShutdownSignal,
    ) -> Self {
        Self {
            store,
            trigger_tx,
            shutdown,
        }
    }

    /// Create a new cron job. Validates the cron expression and computes the
    /// first trigger time.
    pub async fn create_job(
        &self,
        user_id: &str,
        channel: ChannelType,
        schedule: &str,
        prompt: &str,
        run_mode: CronRunMode,
    ) -> Result<CronJob> {
        let normalized = normalize_cron_expression(schedule);
        cron::Schedule::from_str(&normalized)
            .map_err(|e| CronError::InvalidExpression(format!("{schedule}: {e}")))?;

        let now = Utc::now();
        let job = CronJob {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: user_id.to_string(),
            channel,
            schedule: schedule.to_string(),
            prompt: prompt.to_string(),
            status: CronStatus::Enabled,
            run_mode,
            last_triggered_at: None,
            next_trigger_at: compute_next_trigger(schedule, now),
            created_at: now,
            updated_at: now,
        };

        let row = job_to_row(&job)?;
        self.store.create(&row).await.map_err(store_err)?;
        Ok(job)
    }

    /// Delete a cron job by ID.
    pub async fn delete_job(&self, job_id: &str) -> Result<()> {
        self.store.delete(job_id).await.map_err(store_err)?;
        Ok(())
    }

    /// Enable a cron job, recomputing the next trigger time.
    pub async fn enable_job(&self, job_id: &str) -> Result<()> {
        let row = self
            .store
            .get(job_id)
            .await
            .map_err(store_err)?
            .ok_or_else(|| CronError::NotFound(job_id.to_string()))?;
        let mut job = row_to_job(row)?;

        let now = Utc::now();
        job.status = CronStatus::Enabled;
        job.next_trigger_at = compute_next_trigger(&job.schedule, now);
        job.updated_at = now;

        self.store
            .save(&job_to_row(&job)?)
            .await
            .map_err(store_err)?;
        Ok(())
    }

    /// Disable a cron job, clearing its next trigger time.
    pub async fn disable_job(&self, job_id: &str) -> Result<()> {
        let row = self
            .store
            .get(job_id)
            .await
            .map_err(store_err)?
            .ok_or_else(|| CronError::NotFound(job_id.to_string()))?;
        let mut job = row_to_job(row)?;

        let now = Utc::now();
        job.status = CronStatus::Disabled;
        job.next_trigger_at = None;
        job.updated_at = now;

        self.store
            .save(&job_to_row(&job)?)
            .await
            .map_err(store_err)?;
        Ok(())
    }

    /// List all cron jobs for a user.
    pub async fn list_jobs(&self, user_id: &str) -> Result<Vec<CronJob>> {
        self.store
            .list_by_user(user_id)
            .await
            .map_err(store_err)?
            .into_iter()
            .map(row_to_job)
            .collect()
    }

    /// List every cron job regardless of user. Used by operator CLI surfaces
    /// where the invoking identity is a CLI session rather than a per-user
    /// identity.
    pub async fn list_all_jobs(&self) -> Result<Vec<CronJob>> {
        self.store
            .list_all()
            .await
            .map_err(store_err)?
            .into_iter()
            .map(row_to_job)
            .collect()
    }

    /// Fetch a cron job by id, or `None` if it does not exist.
    pub async fn get_job(&self, job_id: &str) -> Result<Option<CronJob>> {
        match self.store.get(job_id).await.map_err(store_err)? {
            Some(row) => Ok(Some(row_to_job(row)?)),
            None => Ok(None),
        }
    }

    /// Manually fire a cron job now, outside the regular schedule.
    ///
    /// Records an execution row (so the run is auditable), dispatches the
    /// trigger event, and does **not** touch the job's `next_trigger_at` —
    /// the normal schedule continues to fire independently.
    pub async fn trigger_now(&self, job_id: &str) -> Result<CronExecution> {
        let row = self
            .store
            .get(job_id)
            .await
            .map_err(store_err)?
            .ok_or_else(|| CronError::NotFound(job_id.to_string()))?;
        let job = row_to_job(row)?;

        let now = Utc::now();
        let execution = CronExecution {
            id: uuid::Uuid::new_v4().to_string(),
            job_id: job.id.clone(),
            user_id: job.user_id.clone(),
            channel: job.channel,
            schedule: job.schedule.clone(),
            prompt: job.prompt.clone(),
            run_mode: job.run_mode.clone(),
            scheduled_fire_time: now,
            triggered_at: now,
            status: ExecutionStatus::Pending,
        };

        let exec_row = execution_to_row(&execution)?;
        self.store
            .record_execution(&exec_row)
            .await
            .map_err(store_err)?;

        let event = CronTriggerEvent {
            job_id: execution.job_id.clone(),
            user_id: execution.user_id.clone(),
            channel: execution.channel,
            prompt: execution.prompt.clone(),
        };

        self.trigger_tx
            .send(event)
            .await
            .map_err(|e| CronError::Storage(format!("failed to dispatch trigger: {e}")))?;

        self.store
            .update_execution_status(&execution.id, "dispatched")
            .await
            .map_err(store_err)?;

        let mut updated = execution;
        updated.status = ExecutionStatus::Dispatched;
        Ok(updated)
    }

    /// List execution records for a job.
    pub async fn list_executions(&self, job_id: &str) -> Result<Vec<CronExecution>> {
        self.store
            .list_executions_by_job(job_id)
            .await
            .map_err(store_err)?
            .into_iter()
            .map(row_to_execution)
            .collect()
    }

    /// Run the background tick loop. Checks for due jobs every 30 seconds
    /// and fires triggers. Exits on shutdown signal.
    pub async fn run(&self) {
        self.recover_pending().await;

        let mut interval = tokio::time::interval(Duration::from_secs(30));
        info!("cron scheduler started");

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    self.tick().await;
                }
                _ = self.shutdown.wait() => {
                    info!("cron scheduler shutting down");
                    break;
                }
            }
        }
    }

    /// Re-dispatch executions that were recorded as `Pending` but never
    /// reached `Dispatched` (crash between record and send).
    async fn recover_pending(&self) {
        let pending_rows = match self.store.list_executions_by_status("pending").await {
            Ok(rows) => rows,
            Err(e) => {
                error!(error = %e, "failed to query pending executions for recovery");
                return;
            }
        };

        for row in pending_rows {
            let exec = match row_to_execution(row) {
                Ok(e) => e,
                Err(e) => {
                    error!(error = %e, "failed to deserialize pending execution");
                    continue;
                }
            };

            info!(
                execution_id = %exec.id,
                job_id = %exec.job_id,
                "re-dispatching pending cron execution after restart"
            );

            let event = CronTriggerEvent {
                job_id: exec.job_id.clone(),
                user_id: exec.user_id.clone(),
                channel: exec.channel,
                prompt: exec.prompt.clone(),
            };

            if let Err(e) = self.trigger_tx.send(event).await {
                error!(execution_id = %exec.id, error = %e, "failed to re-dispatch pending execution");
                continue;
            }

            if let Err(e) = self
                .store
                .update_execution_status(&exec.id, "dispatched")
                .await
            {
                error!(execution_id = %exec.id, error = %e, "failed to mark recovered execution as dispatched");
            }
        }
    }

    async fn tick(&self) {
        let now = Utc::now();
        let now_str = now.to_rfc3339();
        let due_rows = match self.store.list_due(&now_str).await {
            Ok(rows) => rows,
            Err(e) => {
                error!(error = %e, "failed to query due cron jobs");
                return;
            }
        };

        for row in due_rows {
            let mut job = match row_to_job(row) {
                Ok(j) => j,
                Err(e) => {
                    error!(error = %e, "failed to deserialize due cron job");
                    continue;
                }
            };

            // The scheduled_fire_time is the next_trigger_at that was due.
            let scheduled_fire_time = match job.next_trigger_at {
                Some(t) => t,
                None => continue,
            };
            let sft_str = scheduled_fire_time.to_rfc3339();

            // Idempotent: skip if already processed for this schedule slot
            match self
                .store
                .has_execution_for_schedule(&job.id, &sft_str)
                .await
            {
                Ok(true) => {
                    // Already recorded — advance job and skip
                    job.last_triggered_at = Some(now);
                    job.next_trigger_at = compute_next_trigger(&job.schedule, now);
                    job.updated_at = now;
                    if let Ok(updated_row) = job_to_row(&job)
                        && let Err(e) = self.store.save(&updated_row).await
                    {
                        error!(job_id = %job.id, error = %e, "failed to advance duplicate cron job");
                    }
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    error!(job_id = %job.id, error = %e, "failed to check execution dedup");
                    continue;
                }
            }

            // Phase 1: Record execution as Pending
            let execution = CronExecution {
                id: uuid::Uuid::new_v4().to_string(),
                job_id: job.id.clone(),
                user_id: job.user_id.clone(),
                channel: job.channel,
                schedule: job.schedule.clone(),
                prompt: job.prompt.clone(),
                run_mode: job.run_mode.clone(),
                scheduled_fire_time,
                triggered_at: now,
                status: ExecutionStatus::Pending,
            };
            let exec_row = match execution_to_row(&execution) {
                Ok(r) => r,
                Err(e) => {
                    error!(job_id = %job.id, error = %e, "failed to serialize cron execution");
                    continue;
                }
            };
            if let Err(e) = self.store.record_execution(&exec_row).await {
                error!(job_id = %job.id, error = %e, "failed to record cron execution");
                continue;
            }

            // Phase 2: Advance job schedule (before dispatch, so crash won't re-fire)
            if job.is_one_shot() {
                info!(job_id = %job.id, "evicting one-shot cron job after execution");
                if let Err(e) = self.store.delete(&job.id).await {
                    error!(job_id = %job.id, error = %e, "failed to evict one-shot cron job");
                }
            } else {
                job.last_triggered_at = Some(now);
                job.next_trigger_at = compute_next_trigger(&job.schedule, now);
                job.updated_at = now;
                if let Ok(updated_row) = job_to_row(&job)
                    && let Err(e) = self.store.save(&updated_row).await
                {
                    error!(job_id = %job.id, error = %e, "failed to update cron job after trigger");
                }
            }

            // Phase 3: Dispatch trigger
            let event = CronTriggerEvent {
                job_id: execution.job_id.clone(),
                user_id: execution.user_id.clone(),
                channel: execution.channel,
                prompt: execution.prompt.clone(),
            };

            if let Err(e) = self.trigger_tx.send(event).await {
                error!(job_id = %execution.job_id, error = %e, "failed to send cron trigger");
                // Execution stays Pending — will be recovered on next restart
                continue;
            }

            // Phase 4: Mark as Dispatched
            if let Err(e) = self
                .store
                .update_execution_status(&execution.id, "dispatched")
                .await
            {
                error!(execution_id = %execution.id, error = %e, "failed to mark execution as dispatched");
            }
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Normalize a cron expression to the 6/7-field format expected by the `cron` crate.
///
/// Standard 5-field: `min hour dom month dow` → prepend `0` for seconds.
/// 6-field (with seconds) and 7-field (with seconds + year) pass through unchanged.
fn normalize_cron_expression(expression: &str) -> String {
    let fields: Vec<&str> = expression.split_whitespace().collect();
    if fields.len() == 5 {
        format!("0 {expression}")
    } else {
        expression.to_string()
    }
}

/// Compute the next trigger time for a cron expression after the given timestamp.
fn compute_next_trigger(expression: &str, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let normalized = normalize_cron_expression(expression);
    let schedule = cron::Schedule::from_str(&normalized).ok()?;
    schedule.after(&after).next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// In-memory CronStore for testing.
    struct InMemoryCronStore {
        jobs: Mutex<Vec<CronJobRow>>,
        executions: Mutex<Vec<CronExecutionRow>>,
    }

    impl InMemoryCronStore {
        fn new() -> Self {
            Self {
                jobs: Mutex::new(Vec::new()),
                executions: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl CronStore for InMemoryCronStore {
        async fn create(&self, row: &CronJobRow) -> aura_storage::cron::Result<()> {
            self.jobs.lock().unwrap().push(row.clone());
            Ok(())
        }

        async fn get(&self, job_id: &str) -> aura_storage::cron::Result<Option<CronJobRow>> {
            Ok(self
                .jobs
                .lock()
                .unwrap()
                .iter()
                .find(|r| r.id == job_id)
                .cloned())
        }

        async fn save(&self, row: &CronJobRow) -> aura_storage::cron::Result<()> {
            let mut jobs = self.jobs.lock().unwrap();
            if let Some(existing) = jobs.iter_mut().find(|r| r.id == row.id) {
                *existing = row.clone();
                Ok(())
            } else {
                Err(CronStoreError::NotFound(row.id.clone()))
            }
        }

        async fn delete(&self, job_id: &str) -> aura_storage::cron::Result<()> {
            let mut jobs = self.jobs.lock().unwrap();
            let len_before = jobs.len();
            jobs.retain(|r| r.id != job_id);
            if jobs.len() == len_before {
                Err(CronStoreError::NotFound(job_id.to_string()))
            } else {
                Ok(())
            }
        }

        async fn list_by_user(&self, user_id: &str) -> aura_storage::cron::Result<Vec<CronJobRow>> {
            Ok(self
                .jobs
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.user_id == user_id)
                .cloned()
                .collect())
        }

        async fn list_all(&self) -> aura_storage::cron::Result<Vec<CronJobRow>> {
            Ok(self.jobs.lock().unwrap().clone())
        }

        async fn list_enabled(&self) -> aura_storage::cron::Result<Vec<CronJobRow>> {
            Ok(self
                .jobs
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.status == "enabled")
                .cloned()
                .collect())
        }

        async fn list_due(&self, now: &str) -> aura_storage::cron::Result<Vec<CronJobRow>> {
            Ok(self
                .jobs
                .lock()
                .unwrap()
                .iter()
                .filter(|r| {
                    r.status == "enabled"
                        && !r.next_trigger_at.is_empty()
                        && r.next_trigger_at.as_str() <= now
                })
                .cloned()
                .collect())
        }

        async fn record_execution(&self, row: &CronExecutionRow) -> aura_storage::cron::Result<()> {
            self.executions.lock().unwrap().push(row.clone());
            Ok(())
        }

        async fn list_executions_by_job(
            &self,
            job_id: &str,
        ) -> aura_storage::cron::Result<Vec<CronExecutionRow>> {
            Ok(self
                .executions
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.job_id == job_id)
                .cloned()
                .collect())
        }

        async fn list_executions_by_user(
            &self,
            user_id: &str,
        ) -> aura_storage::cron::Result<Vec<CronExecutionRow>> {
            Ok(self
                .executions
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.user_id == user_id)
                .cloned()
                .collect())
        }

        async fn has_execution_for_schedule(
            &self,
            job_id: &str,
            scheduled_fire_time: &str,
        ) -> aura_storage::cron::Result<bool> {
            Ok(self
                .executions
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.job_id == job_id && r.scheduled_fire_time == scheduled_fire_time))
        }

        async fn update_execution_status(
            &self,
            execution_id: &str,
            status: &str,
        ) -> aura_storage::cron::Result<()> {
            let mut execs = self.executions.lock().unwrap();
            if let Some(exec) = execs.iter_mut().find(|r| r.id == execution_id) {
                exec.status = status.to_string();
                Ok(())
            } else {
                Err(CronStoreError::NotFound(execution_id.to_string()))
            }
        }

        async fn list_executions_by_status(
            &self,
            status: &str,
        ) -> aura_storage::cron::Result<Vec<CronExecutionRow>> {
            Ok(self
                .executions
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.status == status)
                .cloned()
                .collect())
        }
    }

    fn make_scheduler(
        store: InMemoryCronStore,
    ) -> (CronScheduler, mpsc::Receiver<CronTriggerEvent>) {
        let (tx, rx) = mpsc::channel(64);
        let scheduler = CronScheduler::new(Box::new(store), tx, ShutdownSignal::new());
        (scheduler, rx)
    }

    #[tokio::test]
    async fn create_job_with_valid_expression() {
        let (scheduler, _rx) = make_scheduler(InMemoryCronStore::new());
        let job = scheduler
            .create_job(
                "u1",
                ChannelType::Cli,
                "0 9 * * *",
                "morning news",
                CronRunMode::Recurring,
            )
            .await
            .unwrap();
        assert_eq!(job.user_id, "u1");
        assert_eq!(job.status, CronStatus::Enabled);
        assert_eq!(job.run_mode, CronRunMode::Recurring);
        assert!(job.next_trigger_at.is_some());
    }

    #[tokio::test]
    async fn create_job_with_invalid_expression() {
        let (scheduler, _rx) = make_scheduler(InMemoryCronStore::new());
        let err = scheduler
            .create_job(
                "u1",
                ChannelType::Cli,
                "not a cron",
                "test",
                CronRunMode::Recurring,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CronError::InvalidExpression(_)));
    }

    #[tokio::test]
    async fn enable_disable_job() {
        let (scheduler, _rx) = make_scheduler(InMemoryCronStore::new());
        let job = scheduler
            .create_job(
                "u1",
                ChannelType::Cli,
                "0 9 * * *",
                "test",
                CronRunMode::Recurring,
            )
            .await
            .unwrap();

        scheduler.disable_job(&job.id).await.unwrap();
        let jobs = scheduler.list_jobs("u1").await.unwrap();
        assert_eq!(jobs[0].status, CronStatus::Disabled);
        assert!(jobs[0].next_trigger_at.is_none());

        scheduler.enable_job(&job.id).await.unwrap();
        let jobs = scheduler.list_jobs("u1").await.unwrap();
        assert_eq!(jobs[0].status, CronStatus::Enabled);
        assert!(jobs[0].next_trigger_at.is_some());
    }

    #[tokio::test]
    async fn tick_fires_due_jobs() {
        let store = InMemoryCronStore::new();
        let (scheduler, mut rx) = make_scheduler(store);

        let job = scheduler
            .create_job(
                "u1",
                ChannelType::Cli,
                "* * * * *",
                "every minute",
                CronRunMode::Recurring,
            )
            .await
            .unwrap();

        // Manually set next_trigger_at to the past so tick() considers it due.
        {
            let past = (Utc::now() - chrono::Duration::seconds(10)).to_rfc3339();
            let mut row = scheduler.store.get(&job.id).await.unwrap().unwrap();
            row.next_trigger_at = past;
            scheduler.store.save(&row).await.unwrap();
        }

        scheduler.tick().await;

        let event = rx.try_recv().unwrap();
        assert_eq!(event.job_id, job.id);
        assert_eq!(event.prompt, "every minute");

        // Verify next_trigger_at was advanced
        let updated_row = scheduler.store.get(&job.id).await.unwrap().unwrap();
        let updated = row_to_job(updated_row).unwrap();
        assert!(updated.last_triggered_at.is_some());
        assert!(updated.next_trigger_at.unwrap() > Utc::now());

        // Verify execution was recorded with Dispatched status
        let execs = scheduler.list_executions(&job.id).await.unwrap();
        assert_eq!(execs.len(), 1);
        assert_eq!(execs[0].job_id, job.id);
        assert_eq!(execs[0].status, ExecutionStatus::Dispatched);
    }

    #[tokio::test]
    async fn tick_does_not_fire_disabled_jobs() {
        let store = InMemoryCronStore::new();
        let (scheduler, mut rx) = make_scheduler(store);

        let job = scheduler
            .create_job(
                "u1",
                ChannelType::Cli,
                "* * * * *",
                "test",
                CronRunMode::Recurring,
            )
            .await
            .unwrap();

        scheduler.disable_job(&job.id).await.unwrap();
        {
            let past = (Utc::now() - chrono::Duration::seconds(10)).to_rfc3339();
            let mut row = scheduler.store.get(&job.id).await.unwrap().unwrap();
            row.next_trigger_at = past;
            scheduler.store.save(&row).await.unwrap();
        }

        scheduler.tick().await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn delete_job_removes_from_store() {
        let (scheduler, _rx) = make_scheduler(InMemoryCronStore::new());
        let job = scheduler
            .create_job(
                "u1",
                ChannelType::Cli,
                "0 9 * * *",
                "test",
                CronRunMode::Recurring,
            )
            .await
            .unwrap();
        scheduler.delete_job(&job.id).await.unwrap();
        let jobs = scheduler.list_jobs("u1").await.unwrap();
        assert!(jobs.is_empty());
    }

    #[tokio::test]
    async fn one_shot_job_evicted_after_firing() {
        let store = InMemoryCronStore::new();
        let (scheduler, mut rx) = make_scheduler(store);

        let job = scheduler
            .create_job(
                "u1",
                ChannelType::Cli,
                "* * * * *",
                "run once",
                CronRunMode::OneShot,
            )
            .await
            .unwrap();
        let job_id = job.id.clone();

        // Set past due
        {
            let past = (Utc::now() - chrono::Duration::seconds(10)).to_rfc3339();
            let mut row = scheduler.store.get(&job_id).await.unwrap().unwrap();
            row.next_trigger_at = past;
            scheduler.store.save(&row).await.unwrap();
        }

        scheduler.tick().await;

        // Trigger was sent
        let event = rx.try_recv().unwrap();
        assert_eq!(event.job_id, job_id);

        // Job was evicted
        assert!(scheduler.store.get(&job_id).await.unwrap().is_none());

        // Execution record preserved
        let execs = scheduler.list_executions(&job_id).await.unwrap();
        assert_eq!(execs.len(), 1);
        assert_eq!(execs[0].run_mode, CronRunMode::OneShot);
    }

    #[test]
    fn row_conversion_round_trip() {
        let job = CronJob {
            id: "cj-rt".to_string(),
            user_id: "u1".to_string(),
            channel: ChannelType::Cli,
            schedule: "0 9 * * *".to_string(),
            prompt: "test".to_string(),
            status: CronStatus::Enabled,
            run_mode: CronRunMode::OneShot,
            last_triggered_at: None,
            next_trigger_at: Some(Utc::now()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let row = job_to_row(&job).unwrap();
        assert_eq!(row.status, "enabled");
        assert!(!row.next_trigger_at.is_empty());

        let restored = row_to_job(row).unwrap();
        assert_eq!(restored.id, "cj-rt");
        assert_eq!(restored.run_mode, CronRunMode::OneShot);
    }

    #[tokio::test]
    async fn tick_idempotent_does_not_double_fire() {
        let store = InMemoryCronStore::new();
        let (scheduler, mut rx) = make_scheduler(store);

        let job = scheduler
            .create_job(
                "u1",
                ChannelType::Cli,
                "* * * * *",
                "dedup test",
                CronRunMode::Recurring,
            )
            .await
            .unwrap();

        let past = (Utc::now() - chrono::Duration::seconds(10)).to_rfc3339();
        {
            let mut row = scheduler.store.get(&job.id).await.unwrap().unwrap();
            row.next_trigger_at = past;
            scheduler.store.save(&row).await.unwrap();
        }

        // First tick fires
        scheduler.tick().await;
        assert!(rx.try_recv().is_ok());

        // Set next_trigger_at back to the same past value to simulate a
        // re-trigger attempt for the same schedule slot.
        let execs = scheduler.list_executions(&job.id).await.unwrap();
        let sft = execs[0].scheduled_fire_time.to_rfc3339();
        {
            let mut row = scheduler.store.get(&job.id).await.unwrap().unwrap();
            row.next_trigger_at = sft;
            scheduler.store.save(&row).await.unwrap();
        }

        // Second tick for the same slot is a no-op (dedup)
        scheduler.tick().await;
        assert!(rx.try_recv().is_err());

        // Still only one execution recorded
        let execs = scheduler.list_executions(&job.id).await.unwrap();
        assert_eq!(execs.len(), 1);
    }

    #[tokio::test]
    async fn recover_pending_re_dispatches() {
        let store = InMemoryCronStore::new();

        // Manually insert a pending execution row (simulating a crash)
        let exec = CronExecution {
            id: "ce-pending".to_string(),
            job_id: "cj-1".to_string(),
            user_id: "u1".to_string(),
            channel: ChannelType::Cli,
            schedule: "* * * * *".to_string(),
            prompt: "recover me".to_string(),
            run_mode: CronRunMode::Recurring,
            scheduled_fire_time: Utc::now(),
            triggered_at: Utc::now(),
            status: ExecutionStatus::Pending,
        };
        let exec_row = execution_to_row(&exec).unwrap();
        store.record_execution(&exec_row).await.unwrap();

        let (scheduler, mut rx) = make_scheduler(store);
        scheduler.recover_pending().await;

        // The pending execution was re-dispatched
        let event = rx.try_recv().unwrap();
        assert_eq!(event.job_id, "cj-1");
        assert_eq!(event.prompt, "recover me");

        // Status updated to dispatched
        let execs = scheduler
            .store
            .list_executions_by_status("dispatched")
            .await
            .unwrap();
        assert_eq!(execs.len(), 1);
        assert_eq!(execs[0].id, "ce-pending");

        // No pending left
        let pending = scheduler
            .store
            .list_executions_by_status("pending")
            .await
            .unwrap();
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn list_all_jobs_returns_every_user() {
        let (scheduler, _rx) = make_scheduler(InMemoryCronStore::new());
        scheduler
            .create_job(
                "u1",
                ChannelType::Cli,
                "0 9 * * *",
                "alice",
                CronRunMode::Recurring,
            )
            .await
            .unwrap();
        scheduler
            .create_job(
                "u2",
                ChannelType::Cli,
                "0 10 * * *",
                "bob",
                CronRunMode::Recurring,
            )
            .await
            .unwrap();

        let all = scheduler.list_all_jobs().await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn get_job_returns_none_when_missing() {
        let (scheduler, _rx) = make_scheduler(InMemoryCronStore::new());
        assert!(scheduler.get_job("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_job_returns_full_job_when_present() {
        let (scheduler, _rx) = make_scheduler(InMemoryCronStore::new());
        let created = scheduler
            .create_job(
                "u1",
                ChannelType::Cli,
                "0 9 * * *",
                "fetch me",
                CronRunMode::Recurring,
            )
            .await
            .unwrap();

        let got = scheduler.get_job(&created.id).await.unwrap().unwrap();
        assert_eq!(got.id, created.id);
        assert_eq!(got.prompt, "fetch me");
    }

    #[tokio::test]
    async fn trigger_now_dispatches_and_records_execution() {
        let (scheduler, mut rx) = make_scheduler(InMemoryCronStore::new());
        let job = scheduler
            .create_job(
                "u1",
                ChannelType::Cli,
                "0 9 * * *",
                "manual fire",
                CronRunMode::Recurring,
            )
            .await
            .unwrap();
        let scheduled_next = job.next_trigger_at;

        let exec = scheduler.trigger_now(&job.id).await.unwrap();
        assert_eq!(exec.job_id, job.id);
        assert_eq!(exec.status, ExecutionStatus::Dispatched);

        let event = rx.try_recv().unwrap();
        assert_eq!(event.job_id, job.id);
        assert_eq!(event.prompt, "manual fire");

        // Manual trigger must not advance the schedule.
        let fetched = scheduler.get_job(&job.id).await.unwrap().unwrap();
        assert_eq!(fetched.next_trigger_at, scheduled_next);
        assert!(fetched.last_triggered_at.is_none());

        // Execution row exists with Dispatched status.
        let execs = scheduler.list_executions(&job.id).await.unwrap();
        assert_eq!(execs.len(), 1);
        assert_eq!(execs[0].status, ExecutionStatus::Dispatched);
    }

    #[tokio::test]
    async fn trigger_now_errors_for_missing_job() {
        let (scheduler, _rx) = make_scheduler(InMemoryCronStore::new());
        let err = scheduler.trigger_now("ghost").await.unwrap_err();
        assert!(matches!(err, CronError::NotFound(_)));
    }

    #[tokio::test]
    async fn execution_row_conversion_round_trip() {
        let exec = CronExecution {
            id: "ce-rt".to_string(),
            job_id: "cj-1".to_string(),
            user_id: "u1".to_string(),
            channel: ChannelType::Cli,
            schedule: "0 9 * * *".to_string(),
            prompt: "test".to_string(),
            run_mode: CronRunMode::Recurring,
            scheduled_fire_time: Utc::now(),
            triggered_at: Utc::now(),
            status: ExecutionStatus::Pending,
        };
        let row = execution_to_row(&exec).unwrap();
        assert_eq!(row.status, "pending");
        assert!(!row.scheduled_fire_time.is_empty());

        let restored = row_to_execution(row).unwrap();
        assert_eq!(restored.id, "ce-rt");
        assert_eq!(restored.status, ExecutionStatus::Pending);
    }
}
