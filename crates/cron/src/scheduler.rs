use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use aura_model::ChannelType;
use aura_storage::{CronExecutionRow, CronJobRow, CronStore, CronStoreError};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

use crate::error::CronError;
use crate::job::{CronExecution, CronJob, CronSchedule, CronStatus, ExecutionStatus};
use crate::shutdown::Shutdown;

// ── Error bridging ────────────────────────────────────────────────────

fn store_err(e: CronStoreError) -> CronError {
    match e {
        CronStoreError::NotFound(id) => CronError::NotFound(id),
        CronStoreError::AlreadyExists(key) => CronError::AlreadyDispatched(key),
        CronStoreError::Internal(msg) => CronError::Storage(msg),
    }
}

type Result<T> = std::result::Result<T, CronError>;

// ── Row ↔ Domain conversion ───────────────────────────────────────────

fn job_to_row(job: &CronJob) -> Result<CronJobRow> {
    let data = serde_json::to_string(job)
        .map_err(|e| CronError::Storage(format!("failed to serialize cron job: {e}")))?;
    Ok(CronJobRow {
        id: job.id.clone(),
        user_id: job.user_id.clone(),
        status: job.status.as_str().to_string(),
        next_trigger_at: job
            .next_trigger_at
            .map(|t| t.timestamp_micros())
            .unwrap_or(0),
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
        scheduled_fire_time: exec.scheduled_fire_time.timestamp_micros(),
        triggered_at: exec.triggered_at.timestamp_micros(),
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
    /// The session that originally registered the cron job (if any).
    /// Symmetric to `create_spawned_session` lineage: lets the
    /// downstream actor stamp `TriggerSource::Cron { origin_session_id }`
    /// on the produced session so trace queries can walk back to
    /// "what user action created this cron job."
    pub origin_session_id: Option<String>,
}

/// Manages cron job lifecycle and runs a background tick loop
/// that fires due jobs on schedule.
pub struct CronScheduler {
    store: Arc<dyn CronStore>,
    trigger_tx: mpsc::Sender<CronTriggerEvent>,
    shutdown: Arc<dyn Shutdown>,
}

/// How often the scheduler wakes up to check for due jobs.
///
/// The underlying `cron` crate resolves expressions to seconds, and one-shot
/// `At` timestamps carry full second precision, so the tick interval is the
/// dominant lower bound on trigger latency. 10s keeps "N seconds after now"
/// reminders usable without burning DB queries on subsecond polling.
const TICK_INTERVAL: Duration = Duration::from_secs(10);

impl CronScheduler {
    pub fn new(
        store: Arc<dyn CronStore>,
        trigger_tx: mpsc::Sender<CronTriggerEvent>,
        shutdown: Arc<dyn Shutdown>,
    ) -> Self {
        Self {
            store,
            trigger_tx,
            shutdown,
        }
    }

    /// Create a new cron job. Validates the schedule and computes the first
    /// trigger time. A `CronSchedule::At` whose time is already in the past
    /// is rejected.
    pub async fn create_job(
        &self,
        user_id: &str,
        channel: ChannelType,
        schedule: CronSchedule,
        prompt: impl Into<String>,
        timezone: String,
        origin_session_id: Option<String>,
    ) -> Result<CronJob> {
        let prompt = prompt.into();
        validate_schedule(&schedule)?;
        let tz = parse_timezone(&timezone)?;

        let now = Utc::now();
        let next_trigger_at = compute_next_trigger(&schedule, tz, now);
        if next_trigger_at.is_none() {
            // Only `At` with past time (Cron is infinite and never returns None here).
            // Surface `now` in both UTC and the caller's timezone so the LLM can
            // immediately self-correct on retry — the typical failure mode is the
            // model not knowing what minute it is and computing `at` slightly into
            // the past.
            return Err(CronError::InvalidSchedule(format!(
                "schedule {} has no future fire time (now is {} / {})",
                schedule.display(),
                now.to_rfc3339(),
                now.with_timezone(&tz).to_rfc3339(),
            )));
        }

        let job = CronJob {
            id: uuid::Uuid::new_v4().to_string(),
            user_id: user_id.to_string(),
            channel,
            schedule,
            prompt,
            timezone,
            status: CronStatus::Enabled,
            last_triggered_at: None,
            next_trigger_at,
            created_at: now,
            updated_at: now,
            origin_session_id,
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

    /// Advance a recurring job to its next fire slot and persist. Used
    /// by both the tick-loop dedup path (already-recorded slot, advance
    /// past it) and the normal-fire path (recompute after dispatch).
    /// Failures are logged but not propagated — like
    /// `mark_one_shot_executed`, the trigger has already gone out and
    /// the row state is best-effort cleanup.
    async fn advance_recurring(&self, job: &mut CronJob, now: DateTime<Utc>) {
        let tz = parse_timezone_or_utc(&job.timezone, &job.id);
        job.last_triggered_at = Some(now);
        job.next_trigger_at = compute_next_trigger(&job.schedule, tz, now);
        job.updated_at = now;
        match job_to_row(job) {
            Ok(row) => {
                if let Err(e) = self.store.save(&row).await {
                    error!(job_id = %job.id, error = %e, "failed to advance cron job after trigger");
                }
            }
            Err(e) => {
                error!(job_id = %job.id, error = %e, "failed to serialize cron job after trigger");
            }
        }
    }

    /// Transition a one-shot job to `Executed` and persist. Shared between
    /// `trigger_now` and the tick loop so manual vs. scheduled firing
    /// produce identical lifecycle effects. Failures are logged but not
    /// propagated — the trigger event has already been dispatched, so
    /// the row state is best-effort cleanup.
    async fn mark_one_shot_executed(&self, job: &mut CronJob, now: DateTime<Utc>) {
        job.status = CronStatus::Executed;
        job.next_trigger_at = None;
        job.last_triggered_at = Some(now);
        job.updated_at = now;
        let row = match job_to_row(job) {
            Ok(r) => r,
            Err(e) => {
                error!(job_id = %job.id, error = %e, "failed to serialize one-shot cron job after fire");
                return;
            }
        };
        if let Err(e) = self.store.save(&row).await {
            error!(job_id = %job.id, error = %e, "failed to mark one-shot cron job as executed");
        }
    }

    /// Enable a cron job, recomputing the next trigger time. Returns an error
    /// if the job is an `At` schedule whose time has already passed — there
    /// is no future fire time to enable.
    pub async fn enable_job(&self, job_id: &str) -> Result<()> {
        let row = self
            .store
            .get(job_id)
            .await
            .map_err(store_err)?
            .ok_or_else(|| CronError::NotFound(job_id.to_string()))?;
        let mut job = row_to_job(row)?;
        let tz = parse_timezone(&job.timezone)?;

        let now = Utc::now();
        let next = compute_next_trigger(&job.schedule, tz, now);
        if next.is_none() {
            return Err(CronError::InvalidSchedule(format!(
                "cannot enable cron job {job_id}: schedule {} has no future fire time \
                 (now is {} / {})",
                job.schedule.display(),
                now.to_rfc3339(),
                now.with_timezone(&tz).to_rfc3339(),
            )));
        }

        job.status = CronStatus::Enabled;
        job.next_trigger_at = next;
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
    /// Records an execution row (so the run is auditable) and dispatches the
    /// trigger event. Recurring (`Cron`) jobs keep their existing
    /// `next_trigger_at` — the normal schedule continues independently.
    /// One-shot (`At`) jobs transition to `CronStatus::Executed` after
    /// dispatch (the row is preserved for history; the `enabled` filter
    /// in `list_due` keeps it from re-firing), matching the tick path so
    /// manual vs scheduled firing have identical lifecycle effects.
    pub async fn trigger_now(&self, job_id: &str) -> Result<CronExecution> {
        let row = self
            .store
            .get(job_id)
            .await
            .map_err(store_err)?
            .ok_or_else(|| CronError::NotFound(job_id.to_string()))?;
        let mut job = row_to_job(row)?;

        let now = Utc::now();
        let execution = CronExecution {
            id: uuid::Uuid::new_v4().to_string(),
            job_id: job.id.clone(),
            user_id: job.user_id.clone(),
            channel: job.channel.clone(),
            schedule: job.schedule.clone(),
            prompt: job.prompt.clone(),
            scheduled_fire_time: now,
            triggered_at: now,
            status: ExecutionStatus::Pending,
            origin_session_id: job.origin_session_id.clone(),
        };

        let exec_row = execution_to_row(&execution)?;
        self.store
            .record_execution(&exec_row)
            .await
            .map_err(store_err)?;

        let event = CronTriggerEvent {
            job_id: execution.job_id.clone(),
            user_id: execution.user_id.clone(),
            channel: execution.channel.clone(),
            prompt: execution.prompt.clone(),
            origin_session_id: execution.origin_session_id.clone(),
        };

        self.trigger_tx
            .send(event)
            .await
            .map_err(|e| CronError::Storage(format!("failed to dispatch trigger: {e}")))?;

        self.store
            .update_execution_status(&execution.id, "dispatched")
            .await
            .map_err(store_err)?;

        if job.is_one_shot() {
            info!(job_id = %job.id, "marking one-shot cron job as executed after manual trigger");
            self.mark_one_shot_executed(&mut job, now).await;
        }

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

    /// Run the background tick loop. Checks for due jobs at every tick and
    /// fires triggers. Exits on shutdown signal.
    pub async fn run(&self) {
        self.recover_pending().await;

        let mut interval = tokio::time::interval(TICK_INTERVAL);
        info!(
            tick_secs = TICK_INTERVAL.as_secs(),
            "cron scheduler started"
        );

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
                origin_session_id: exec.origin_session_id.clone(),
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
        let due_rows = match self.store.list_due(now.timestamp_micros()).await {
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

            // Idempotent: skip if already processed for this schedule slot
            match self
                .store
                .has_execution_for_schedule(&job.id, scheduled_fire_time.timestamp_micros())
                .await
            {
                Ok(true) => {
                    // Already recorded — advance past the slot and skip
                    self.advance_recurring(&mut job, now).await;
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
                channel: job.channel.clone(),
                schedule: job.schedule.clone(),
                prompt: job.prompt.clone(),
                scheduled_fire_time,
                triggered_at: now,
                origin_session_id: job.origin_session_id.clone(),
                status: ExecutionStatus::Pending,
            };
            let exec_row = match execution_to_row(&execution) {
                Ok(r) => r,
                Err(e) => {
                    error!(job_id = %job.id, error = %e, "failed to serialize cron execution");
                    continue;
                }
            };
            match self.store.record_execution(&exec_row).await {
                Ok(()) => {}
                Err(CronStoreError::AlreadyExists(key)) => {
                    debug!(job_id = %job.id, slot = %key, "skipping duplicate cron execution slot");
                    continue;
                }
                Err(e) => {
                    error!(job_id = %job.id, error = %e, "failed to record cron execution");
                    continue;
                }
            }

            // Phase 2: Advance job schedule (before dispatch, so crash won't re-fire)
            if job.is_one_shot() {
                info!(job_id = %job.id, "marking one-shot cron job as executed");
                self.mark_one_shot_executed(&mut job, now).await;
            } else {
                self.advance_recurring(&mut job, now).await;
            }

            // Phase 3: Dispatch trigger
            let event = CronTriggerEvent {
                job_id: execution.job_id.clone(),
                user_id: execution.user_id.clone(),
                channel: execution.channel,
                prompt: execution.prompt.clone(),
                origin_session_id: execution.origin_session_id.clone(),
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

/// Parse-validate a schedule at creation time. Does not check whether the
/// schedule has a future fire time — that's `compute_next_trigger`'s job.
fn validate_schedule(schedule: &CronSchedule) -> Result<()> {
    match schedule {
        CronSchedule::Cron { expr } => {
            let normalized = normalize_cron_expression(expr);
            cron::Schedule::from_str(&normalized)
                .map(|_| ())
                .map_err(|e| CronError::InvalidSchedule(format!("{expr}: {e}")))
        }
        CronSchedule::At { .. } => Ok(()),
    }
}

/// Compute the next trigger time for a schedule after the given timestamp.
///
/// - `Cron(expr)` returns the next matching tick **interpreted in `tz`**, then
///   converted back to UTC for storage. So `0 9 * * *` with `tz = Asia/Shanghai`
///   fires at 09:00 Shanghai time daily, not 09:00 UTC. Returns `None` only
///   if the underlying cron parser fails (caught earlier by `validate_schedule`).
/// - `At(time)` ignores `tz` (the timestamp is already absolute UTC) and
///   returns `Some(time)` iff strictly in the future.
fn compute_next_trigger(
    schedule: &CronSchedule,
    tz: Tz,
    after: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    match schedule {
        CronSchedule::Cron { expr } => {
            let normalized = normalize_cron_expression(expr);
            let parsed = cron::Schedule::from_str(&normalized).ok()?;
            parsed
                .after(&after.with_timezone(&tz))
                .next()
                .map(|t| t.with_timezone(&Utc))
        }
        CronSchedule::At { time } => (*time > after).then_some(*time),
    }
}

/// Parse an IANA timezone string, mapping failure to `InvalidSchedule`
/// (the user asked for a timezone we cannot evaluate against).
fn parse_timezone(name: &str) -> Result<Tz> {
    name.parse::<Tz>()
        .map_err(|e| CronError::InvalidSchedule(format!("invalid timezone {name}: {e}")))
}

/// Parse a timezone for a stored job. Falls back to UTC and warns —
/// the row was already accepted at creation time, so we never want a
/// rare bad name (e.g. operator hand-edited the row) to silently
/// stop the tick loop from advancing other jobs.
fn parse_timezone_or_utc(name: &str, job_id: &str) -> Tz {
    match name.parse::<Tz>() {
        Ok(tz) => tz,
        Err(e) => {
            tracing::warn!(
                job_id, timezone = name, error = %e,
                "stored cron job has unparseable timezone; falling back to UTC for this fire"
            );
            chrono_tz::UTC
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shutdown::NeverShutdown;
    use async_trait::async_trait;
    use parking_lot::Mutex;

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
            self.jobs.lock().push(row.clone());
            Ok(())
        }

        async fn get(&self, job_id: &str) -> aura_storage::cron::Result<Option<CronJobRow>> {
            Ok(self.jobs.lock().iter().find(|r| r.id == job_id).cloned())
        }

        async fn save(&self, row: &CronJobRow) -> aura_storage::cron::Result<()> {
            let mut jobs = self.jobs.lock();
            if let Some(existing) = jobs.iter_mut().find(|r| r.id == row.id) {
                *existing = row.clone();
                Ok(())
            } else {
                Err(CronStoreError::NotFound(row.id.clone()))
            }
        }

        async fn delete(&self, job_id: &str) -> aura_storage::cron::Result<()> {
            let mut jobs = self.jobs.lock();
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
                .iter()
                .filter(|r| r.user_id == user_id)
                .cloned()
                .collect())
        }

        async fn list_all(&self) -> aura_storage::cron::Result<Vec<CronJobRow>> {
            Ok(self.jobs.lock().to_vec())
        }

        async fn list_enabled(&self) -> aura_storage::cron::Result<Vec<CronJobRow>> {
            Ok(self
                .jobs
                .lock()
                .iter()
                .filter(|r| r.status == "enabled")
                .cloned()
                .collect())
        }

        async fn list_due(&self, now_us: i64) -> aura_storage::cron::Result<Vec<CronJobRow>> {
            Ok(self
                .jobs
                .lock()
                .iter()
                .filter(|r| {
                    r.status == "enabled" && r.next_trigger_at != 0 && r.next_trigger_at <= now_us
                })
                .cloned()
                .collect())
        }

        async fn record_execution(&self, row: &CronExecutionRow) -> aura_storage::cron::Result<()> {
            self.executions.lock().push(row.clone());
            Ok(())
        }

        async fn list_executions_by_job(
            &self,
            job_id: &str,
        ) -> aura_storage::cron::Result<Vec<CronExecutionRow>> {
            Ok(self
                .executions
                .lock()
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
                .iter()
                .filter(|r| r.user_id == user_id)
                .cloned()
                .collect())
        }

        async fn has_execution_for_schedule(
            &self,
            job_id: &str,
            scheduled_fire_time_us: i64,
        ) -> aura_storage::cron::Result<bool> {
            Ok(self
                .executions
                .lock()
                .iter()
                .any(|r| r.job_id == job_id && r.scheduled_fire_time == scheduled_fire_time_us))
        }

        async fn update_execution_status(
            &self,
            execution_id: &str,
            status: &str,
        ) -> aura_storage::cron::Result<()> {
            let mut execs = self.executions.lock();
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
        let scheduler = CronScheduler::new(Arc::new(store), tx, Arc::new(NeverShutdown));
        (scheduler, rx)
    }

    /// Helper: create a prompt-action cron job.
    async fn create_prompt_cron(
        scheduler: &CronScheduler,
        user_id: &str,
        expr: &str,
        prompt: &str,
    ) -> CronJob {
        scheduler
            .create_job(
                user_id,
                ChannelType::tui(),
                CronSchedule::cron(expr),
                prompt,
                "UTC".to_string(),
                None,
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn create_job_with_valid_cron() {
        let (scheduler, _rx) = make_scheduler(InMemoryCronStore::new());
        let job = create_prompt_cron(&scheduler, "u1", "0 9 * * *", "morning news").await;
        assert_eq!(job.user_id, "u1");
        assert_eq!(job.status, CronStatus::Enabled);
        assert!(!job.is_one_shot());
        assert!(job.next_trigger_at.is_some());
    }

    #[tokio::test]
    async fn create_job_with_future_at() {
        let (scheduler, _rx) = make_scheduler(InMemoryCronStore::new());
        let fire_at = Utc::now() + chrono::Duration::minutes(5);
        let job = scheduler
            .create_job(
                "u1",
                ChannelType::tui(),
                CronSchedule::at(fire_at),
                "later",
                "UTC".to_string(),
                None,
            )
            .await
            .unwrap();
        assert!(job.is_one_shot());
        assert_eq!(job.next_trigger_at, Some(fire_at));
    }

    #[tokio::test]
    async fn create_job_with_past_at_rejected() {
        let (scheduler, _rx) = make_scheduler(InMemoryCronStore::new());
        let past = Utc::now() - chrono::Duration::minutes(1);
        let err = scheduler
            .create_job(
                "u1",
                ChannelType::tui(),
                CronSchedule::at(past),
                "too late",
                "Asia/Shanghai".to_string(),
                None,
            )
            .await
            .unwrap_err();
        // Error message must surface "now" so the LLM can self-correct on
        // retry — the typical failure mode is the model not knowing the
        // current minute and computing `at` slightly into the past.
        let msg = err.to_string();
        assert!(matches!(err, CronError::InvalidSchedule(_)), "{msg}");
        assert!(msg.contains("now is"), "missing now hint: {msg}");
        // Surfaces both the UTC instant and the wall-clock time in the
        // caller's timezone so the LLM doesn't need to convert.
        assert!(msg.contains("+00:00"), "missing UTC offset: {msg}");
        assert!(msg.contains("+08:00"), "missing tz-localised time: {msg}");
    }

    #[tokio::test]
    async fn create_job_with_invalid_cron_expression() {
        let (scheduler, _rx) = make_scheduler(InMemoryCronStore::new());
        let err = scheduler
            .create_job(
                "u1",
                ChannelType::tui(),
                CronSchedule::cron("not a cron"),
                "test",
                "UTC".to_string(),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CronError::InvalidSchedule(_)));
    }

    #[tokio::test]
    async fn create_job_honors_timezone_for_cron_expression() {
        // `0 9 * * *` in Asia/Shanghai (UTC+08) should fire at 01:00 UTC,
        // not 09:00 UTC. This is the bug the timezone field exists to
        // fix; the test pins the contract.
        use chrono::Timelike;
        let (scheduler, _rx) = make_scheduler(InMemoryCronStore::new());
        let job = scheduler
            .create_job(
                "u1",
                ChannelType::tui(),
                CronSchedule::cron("0 9 * * *"),
                "morning",
                "Asia/Shanghai".to_string(),
                None,
            )
            .await
            .unwrap();
        let next = job.next_trigger_at.expect("must have next trigger");
        assert_eq!(next.hour(), 1, "9am Shanghai = 1am UTC, got {next}");
        assert_eq!(next.minute(), 0);
    }

    #[tokio::test]
    async fn create_job_rejects_invalid_timezone() {
        let (scheduler, _rx) = make_scheduler(InMemoryCronStore::new());
        let err = scheduler
            .create_job(
                "u1",
                ChannelType::tui(),
                CronSchedule::cron("0 9 * * *"),
                "x",
                "Mars/Olympus_Mons".to_string(),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CronError::InvalidSchedule(_)));
    }

    #[tokio::test]
    async fn enable_disable_job() {
        let (scheduler, _rx) = make_scheduler(InMemoryCronStore::new());
        let job = create_prompt_cron(&scheduler, "u1", "0 9 * * *", "test").await;

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
    async fn enable_expired_at_job_rejected() {
        let (scheduler, _rx) = make_scheduler(InMemoryCronStore::new());
        let fire_at = Utc::now() + chrono::Duration::seconds(30);
        let job = scheduler
            .create_job(
                "u1",
                ChannelType::tui(),
                CronSchedule::at(fire_at),
                "later",
                "UTC".to_string(),
                None,
            )
            .await
            .unwrap();
        scheduler.disable_job(&job.id).await.unwrap();

        // Simulate passage of time past the fire point by rewriting the row.
        let mut row = scheduler.store.get(&job.id).await.unwrap().unwrap();
        let stored: CronJob = serde_json::from_str(&row.data).unwrap();
        let expired = CronJob {
            schedule: CronSchedule::at(Utc::now() - chrono::Duration::seconds(10)),
            ..stored
        };
        row.data = serde_json::to_string(&expired).unwrap();
        scheduler.store.save(&row).await.unwrap();

        let err = scheduler.enable_job(&job.id).await.unwrap_err();
        assert!(matches!(err, CronError::InvalidSchedule(_)));
    }

    #[tokio::test]
    async fn tick_fires_due_jobs() {
        let store = InMemoryCronStore::new();
        let (scheduler, mut rx) = make_scheduler(store);

        let job = create_prompt_cron(&scheduler, "u1", "* * * * *", "every minute").await;

        // Manually set next_trigger_at to the past so tick() considers it due.
        {
            let past = (Utc::now() - chrono::Duration::seconds(10)).timestamp_micros();
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

        let job = create_prompt_cron(&scheduler, "u1", "* * * * *", "test").await;

        scheduler.disable_job(&job.id).await.unwrap();
        {
            let past = (Utc::now() - chrono::Duration::seconds(10)).timestamp_micros();
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
        let job = create_prompt_cron(&scheduler, "u1", "0 9 * * *", "test").await;
        scheduler.delete_job(&job.id).await.unwrap();
        let jobs = scheduler.list_jobs("u1").await.unwrap();
        assert!(jobs.is_empty());
    }

    #[tokio::test]
    async fn at_job_marked_executed_after_firing() {
        let store = InMemoryCronStore::new();
        let (scheduler, mut rx) = make_scheduler(store);

        let fire_at = Utc::now() + chrono::Duration::seconds(30);
        let job = scheduler
            .create_job(
                "u1",
                ChannelType::tui(),
                CronSchedule::at(fire_at),
                "run once",
                "UTC".to_string(),
                None,
            )
            .await
            .unwrap();
        let job_id = job.id.clone();

        // Set past due
        {
            let past = (Utc::now() - chrono::Duration::seconds(10)).timestamp_micros();
            let mut row = scheduler.store.get(&job_id).await.unwrap().unwrap();
            row.next_trigger_at = past;
            scheduler.store.save(&row).await.unwrap();
        }

        scheduler.tick().await;

        // Trigger was sent
        let event = rx.try_recv().unwrap();
        assert_eq!(event.job_id, job_id);

        // Row preserved with Executed status — `list_due` filter on
        // `status = 'enabled'` keeps it from re-firing.
        let fetched = scheduler.get_job(&job_id).await.unwrap().unwrap();
        assert_eq!(fetched.status, CronStatus::Executed);
        assert!(fetched.next_trigger_at.is_none());
        assert!(fetched.last_triggered_at.is_some());

        // Execution record preserved
        let execs = scheduler.list_executions(&job_id).await.unwrap();
        assert_eq!(execs.len(), 1);
        assert!(execs[0].schedule.is_one_shot());
    }

    #[test]
    fn row_conversion_round_trip() {
        let job = CronJob {
            id: "cj-rt".to_string(),
            user_id: "u1".to_string(),
            channel: ChannelType::tui(),
            schedule: CronSchedule::cron("0 9 * * *"),
            prompt: "test".to_string(),
            timezone: "UTC".to_string(),
            status: CronStatus::Enabled,
            last_triggered_at: None,
            next_trigger_at: Some(Utc::now()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            origin_session_id: None,
        };
        let row = job_to_row(&job).unwrap();
        assert_eq!(row.status, "enabled");
        assert_ne!(row.next_trigger_at, 0);

        let restored = row_to_job(row).unwrap();
        assert_eq!(restored.id, "cj-rt");
        assert!(!restored.is_one_shot());
    }

    #[tokio::test]
    async fn tick_idempotent_does_not_double_fire() {
        let store = InMemoryCronStore::new();
        let (scheduler, mut rx) = make_scheduler(store);

        let job = create_prompt_cron(&scheduler, "u1", "* * * * *", "dedup test").await;

        let past = (Utc::now() - chrono::Duration::seconds(10)).timestamp_micros();
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
        let sft = execs[0].scheduled_fire_time.timestamp_micros();
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
            channel: ChannelType::tui(),
            schedule: CronSchedule::cron("* * * * *"),
            prompt: "recover me".to_string(),
            scheduled_fire_time: Utc::now(),
            triggered_at: Utc::now(),
            status: ExecutionStatus::Pending,
            origin_session_id: None,
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
        create_prompt_cron(&scheduler, "u1", "0 9 * * *", "alice").await;
        create_prompt_cron(&scheduler, "u2", "0 10 * * *", "bob").await;

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
        let created = create_prompt_cron(&scheduler, "u1", "0 9 * * *", "fetch me").await;

        let got = scheduler.get_job(&created.id).await.unwrap().unwrap();
        assert_eq!(got.id, created.id);
        assert_eq!(got.prompt, "fetch me");
    }

    #[tokio::test]
    async fn trigger_now_dispatches_and_records_execution() {
        let (scheduler, mut rx) = make_scheduler(InMemoryCronStore::new());
        let job = create_prompt_cron(&scheduler, "u1", "0 9 * * *", "manual fire").await;
        let scheduled_next = job.next_trigger_at;

        let exec = scheduler.trigger_now(&job.id).await.unwrap();
        assert_eq!(exec.job_id, job.id);
        assert_eq!(exec.status, ExecutionStatus::Dispatched);

        let event = rx.try_recv().unwrap();
        assert_eq!(event.job_id, job.id);
        assert_eq!(event.prompt, "manual fire");

        // Recurring job preserved, schedule unchanged.
        let fetched = scheduler.get_job(&job.id).await.unwrap().unwrap();
        assert_eq!(fetched.next_trigger_at, scheduled_next);
        assert!(fetched.last_triggered_at.is_none());

        // Execution row exists with Dispatched status.
        let execs = scheduler.list_executions(&job.id).await.unwrap();
        assert_eq!(execs.len(), 1);
        assert_eq!(execs[0].status, ExecutionStatus::Dispatched);
    }

    #[tokio::test]
    async fn trigger_now_marks_at_job_executed() {
        let (scheduler, mut rx) = make_scheduler(InMemoryCronStore::new());
        let fire_at = Utc::now() + chrono::Duration::minutes(5);
        let job = scheduler
            .create_job(
                "u1",
                ChannelType::tui(),
                CronSchedule::at(fire_at),
                "manual one-shot",
                "UTC".to_string(),
                None,
            )
            .await
            .unwrap();

        let exec = scheduler.trigger_now(&job.id).await.unwrap();
        assert_eq!(exec.status, ExecutionStatus::Dispatched);
        assert!(rx.try_recv().is_ok());

        // Row preserved with Executed status; execution kept.
        let fetched = scheduler.get_job(&job.id).await.unwrap().unwrap();
        assert_eq!(fetched.status, CronStatus::Executed);
        assert!(fetched.next_trigger_at.is_none());
        assert!(fetched.last_triggered_at.is_some());
        let execs = scheduler.list_executions(&job.id).await.unwrap();
        assert_eq!(execs.len(), 1);
    }

    #[tokio::test]
    async fn trigger_now_errors_for_missing_job() {
        let (scheduler, _rx) = make_scheduler(InMemoryCronStore::new());
        let err = scheduler.trigger_now("ghost").await.unwrap_err();
        assert!(matches!(err, CronError::NotFound(_)));
    }

    #[tokio::test]
    async fn trigger_carries_origin_session_id_through_event() {
        let (scheduler, mut rx) = make_scheduler(InMemoryCronStore::new());
        let future = Utc::now() + chrono::Duration::hours(1);
        let job = scheduler
            .create_job(
                "u1",
                ChannelType::tui(),
                CronSchedule::at(future),
                "lineage carries",
                "UTC".to_string(),
                Some("sess-creator".into()),
            )
            .await
            .unwrap();
        scheduler.trigger_now(&job.id).await.unwrap();
        let event = rx.try_recv().expect("trigger event must land");
        assert_eq!(event.origin_session_id.as_deref(), Some("sess-creator"));
    }

    #[tokio::test]
    async fn execution_row_conversion_round_trip() {
        let exec = CronExecution {
            id: "ce-rt".to_string(),
            job_id: "cj-1".to_string(),
            user_id: "u1".to_string(),
            channel: ChannelType::tui(),
            schedule: CronSchedule::cron("0 9 * * *"),
            prompt: "test".to_string(),
            scheduled_fire_time: Utc::now(),
            triggered_at: Utc::now(),
            status: ExecutionStatus::Pending,
            origin_session_id: None,
        };
        let row = execution_to_row(&exec).unwrap();
        assert_eq!(row.status, "pending");
        assert_ne!(row.scheduled_fire_time, 0);

        let restored = row_to_execution(row).unwrap();
        assert_eq!(restored.id, "ce-rt");
        assert_eq!(restored.status, ExecutionStatus::Pending);
    }
}
