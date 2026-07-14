use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use baybo_model::{ChannelType, SessionId};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

use crate::error::CronError;
use crate::shutdown::Shutdown;
use baybo_model::{CronExecution, CronJob, CronSchedule, CronStatus, ExecutionStatus};
use baybo_store::{CronStore, ExecutionCompletion};

type Result<T> = std::result::Result<T, CronError>;

// ── Scheduler ──────────────────────────────────────────────────────────

/// Event emitted when a cron job fires.
#[derive(Debug, Clone)]
pub struct CronTriggerEvent {
    pub job_id: String,
    /// The execution row this fire is recorded under. The agent layer stamps
    /// the fire's outcome onto it and uses it as the idempotency key when
    /// delivering a one-shot's result to the origin conversation.
    pub execution_id: String,
    pub user_id: String,
    pub channel: ChannelType,
    /// The job's display title — names the fire's conversation (recurring)
    /// and heads its notification (one-shot).
    pub title: String,
    /// The job's IANA timezone — dates the fire's conversation in the zone the
    /// user scheduled it in.
    pub timezone: String,
    pub prompt: String,
    /// Whether the job fires exactly once ([`CronSchedule::At`]). Decides how
    /// its result is delivered: a one-shot notifies the origin conversation
    /// and emits nothing under its own session; a recurring fire *is* the
    /// conversation and dispatches normally.
    pub one_shot: bool,
    /// The session that originally registered the cron job (if any).
    /// Symmetric to `create_spawned_session` lineage: lets the
    /// downstream actor stamp `TriggerSource::Cron { origin_session_id }`
    /// on the produced session so trace queries can walk back to
    /// "what user action created this cron job" — and, for a one-shot, so
    /// the fire's result can be delivered back into that conversation.
    pub origin_session_id: Option<SessionId>,
}

impl CronTriggerEvent {
    /// The fire event for a recorded execution. Everything the agent layer
    /// needs rides on the execution snapshot, so a job edited or deleted
    /// between record and dispatch can't change what this fire does.
    fn for_execution(execution: &CronExecution) -> Self {
        Self {
            job_id: execution.job_id.clone(),
            execution_id: execution.id.clone(),
            user_id: execution.user_id.clone(),
            channel: execution.channel.clone(),
            title: execution.display_title(),
            timezone: execution.timezone.clone(),
            prompt: execution.prompt.clone(),
            one_shot: execution.is_one_shot(),
            origin_session_id: execution.origin_session_id.clone(),
        }
    }
}

/// Everything needed to create a cron job. A struct rather than seven
/// positional arguments — `title` / `prompt` / `timezone` are all strings and
/// transposing them at a call site would compile.
#[derive(Debug, Clone)]
pub struct NewCronJob {
    pub user_id: String,
    pub channel: ChannelType,
    /// Short human name for the job (`CronCreate` requires one).
    pub title: String,
    pub schedule: CronSchedule,
    pub prompt: String,
    /// IANA timezone the schedule is evaluated in.
    pub timezone: String,
    /// The conversation this job was created from. For a one-shot, it is also
    /// where the fire's result will be delivered.
    pub origin_session_id: Option<SessionId>,
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
    pub async fn create_job(&self, spec: NewCronJob) -> Result<CronJob> {
        let NewCronJob {
            user_id,
            channel,
            title,
            schedule,
            prompt,
            timezone,
            origin_session_id,
        } = spec;
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
            user_id,
            channel,
            title,
            schedule,
            prompt,
            timezone,
            status: CronStatus::Enabled,
            last_triggered_at: None,
            next_trigger_at,
            created_at: now,
            updated_at: now,
            origin_session_id,
            deleted_at: None,
        };

        self.store.create(&job).await?;
        Ok(job)
    }

    /// Stamp a fire's terminal state onto its execution row — see
    /// [`CronStore::record_execution_completion`]. Called by the agent layer's
    /// cron waiter before it delivers the result.
    pub async fn record_execution_completion(
        &self,
        execution_id: &str,
        completion: ExecutionCompletion,
    ) -> Result<()> {
        self.store
            .record_execution_completion(execution_id, completion)
            .await
            .map_err(CronError::from)
    }

    /// Mark a one-shot's result delivered (or terminally dropped) — see
    /// [`CronStore::mark_execution_notified`].
    pub async fn mark_execution_notified(
        &self,
        execution_id: &str,
        at: DateTime<Utc>,
    ) -> Result<()> {
        self.store
            .mark_execution_notified(execution_id, at)
            .await
            .map_err(CronError::from)
    }

    /// One-shot executions whose fire completed but whose result never
    /// reached the origin conversation (crash in the delivery window). The
    /// agent layer re-drives these at boot; recurring fires are excluded —
    /// their result lives in their own conversation, with nothing to deliver.
    pub async fn list_executions_awaiting_delivery(&self) -> Result<Vec<CronExecution>> {
        let rows = self.store.list_executions_awaiting_delivery().await?;
        Ok(rows
            .into_iter()
            .filter(CronExecution::is_one_shot)
            .collect())
    }

    /// Move a cron job to the recycle bin. It stops firing and drops out of
    /// every listing, but the row survives and stays resolvable by
    /// [`Self::get_job`] — its execution records keep naming a real job, and
    /// [`Self::restore_job`] can bring it back.
    pub async fn delete_job(&self, job_id: &str) -> Result<()> {
        self.store.delete(job_id).await.map_err(CronError::from)
    }

    /// Bring a job back from the recycle bin, with the status it was deleted
    /// with.
    ///
    /// A job restored after slots came and went while it sat in the bin must
    /// not fire for them: an enabled job's `next_trigger_at` is recomputed from
    /// now — never back-filled, never a past instant — and an `At` one-shot
    /// whose moment passed while deleted comes back disabled, since it has no
    /// fire time left. The recomputed schedule is written **before** the row
    /// becomes visible again, so a concurrent tick can never observe it live
    /// with a stale slot.
    pub async fn restore_job(&self, job_id: &str) -> Result<()> {
        let mut job = self
            .store
            .get(job_id)
            .await?
            .ok_or_else(|| CronError::NotFound(job_id.to_string()))?;
        if !job.is_deleted() {
            return Ok(());
        }

        let now = Utc::now();
        let tz = parse_timezone_or_utc(&job.timezone, &job.id);
        job.next_trigger_at = match job.status {
            CronStatus::Enabled => compute_next_trigger(&job.schedule, tz, now),
            CronStatus::Disabled | CronStatus::Executed => None,
        };
        if job.status == CronStatus::Enabled && job.next_trigger_at.is_none() {
            info!(job_id = %job.id, "restored one-shot has no fire time left; restoring it disabled");
            job.status = CronStatus::Disabled;
        }
        job.updated_at = now;
        self.store.save(&job).await?;

        self.store.restore(job_id).await.map_err(CronError::from)
    }

    /// The recycle bin: every soft-deleted job, most recently deleted first.
    pub async fn list_deleted_jobs(&self) -> Result<Vec<CronJob>> {
        self.store.list_deleted().await.map_err(CronError::from)
    }

    /// Advance a recurring job to its next fire slot and persist. Used
    /// by both the tick-loop dedup path (already-recorded slot, advance
    /// past it) and the normal-fire path (recompute after dispatch).
    /// Failures are logged but not propagated — like
    /// `mark_one_shot_executed`, the trigger has already gone out and
    /// the row state is best-effort cleanup.
    ///
    /// The write is conditional on the row still being the enabled, live job
    /// this snapshot was read as: a pause or a delete that lands while the
    /// slot is being fired must not be overwritten by the advance, or the job
    /// the user just stopped is re-armed and keeps firing.
    async fn advance_recurring(&self, job: &mut CronJob, now: DateTime<Utc>) {
        let tz = parse_timezone_or_utc(&job.timezone, &job.id);
        job.last_triggered_at = Some(now);
        job.next_trigger_at = compute_next_trigger(&job.schedule, tz, now);
        job.updated_at = now;
        match self.store.save_if_still_enabled(job).await {
            Ok(true) => {}
            Ok(false) => {
                debug!(job_id = %job.id, "cron job was paused or deleted mid-fire; not re-arming it")
            }
            Err(e) => {
                error!(job_id = %job.id, error = %e, "failed to advance cron job after trigger")
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
        if let Err(e) = self.store.save(job).await {
            error!(job_id = %job.id, error = %e, "failed to mark one-shot cron job as executed");
        }
    }

    /// Enable a cron job, recomputing the next trigger time. Returns an error
    /// if the job is an `At` schedule whose time has already passed — there
    /// is no future fire time to enable.
    ///
    /// A job in the recycle bin reads as absent, the way it does in every
    /// listing: resuming it could not put it back on the schedule anyway
    /// (`list_due` never sees it), so reporting success would promise a fire
    /// that can never happen. `restore_job` is the only way back.
    pub async fn enable_job(&self, job_id: &str) -> Result<()> {
        let mut job = self
            .store
            .get(job_id)
            .await?
            .filter(|job| !job.is_deleted())
            .ok_or_else(|| CronError::NotFound(job_id.to_string()))?;
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

        self.store.save(&job).await.map_err(CronError::from)
    }

    /// Disable a cron job, clearing its next trigger time.
    ///
    /// A job in the recycle bin reads as absent: it is already stopped, and
    /// pausing it would rewrite the status `restore_job` promises to bring it
    /// back with.
    pub async fn disable_job(&self, job_id: &str) -> Result<()> {
        let mut job = self
            .store
            .get(job_id)
            .await?
            .filter(|job| !job.is_deleted())
            .ok_or_else(|| CronError::NotFound(job_id.to_string()))?;

        let now = Utc::now();
        job.status = CronStatus::Disabled;
        job.next_trigger_at = None;
        job.updated_at = now;

        self.store.save(&job).await.map_err(CronError::from)
    }

    /// List all cron jobs for a user.
    pub async fn list_jobs(&self, user_id: &str) -> Result<Vec<CronJob>> {
        self.store
            .list_by_user(user_id)
            .await
            .map_err(CronError::from)
    }

    /// List every cron job regardless of user. Used by operator CLI surfaces
    /// where the invoking identity is a CLI session rather than a per-user
    /// identity.
    pub async fn list_all_jobs(&self) -> Result<Vec<CronJob>> {
        self.store.list_all().await.map_err(CronError::from)
    }

    /// Fetch a cron job by id, or `None` if it does not exist.
    pub async fn get_job(&self, job_id: &str) -> Result<Option<CronJob>> {
        self.store.get(job_id).await.map_err(CronError::from)
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
    ///
    /// A job in the recycle bin does not fire, on demand no more than on
    /// schedule: it reads as absent here, the way it does in every listing.
    pub async fn trigger_now(&self, job_id: &str) -> Result<CronExecution> {
        let mut job = self
            .store
            .get(job_id)
            .await?
            .filter(|job| !job.is_deleted())
            .ok_or_else(|| CronError::NotFound(job_id.to_string()))?;

        let now = Utc::now();
        let execution = CronExecution::pending(&job, now, now);

        self.store.record_execution(&execution).await?;

        self.trigger_tx
            .send(CronTriggerEvent::for_execution(&execution))
            .await
            .map_err(|e| CronError::Storage(format!("failed to dispatch trigger: {e}")))?;

        self.store
            .update_execution_status(&execution.id, ExecutionStatus::Dispatched)
            .await?;

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
            .map_err(CronError::from)
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
    ///
    /// A `Pending` execution has never run — re-dispatching it *fires* the
    /// job, so a job the user deleted in the meantime must be skipped here
    /// exactly as it is in `list_due`. (This is not the delivery re-drive,
    /// which replays an already-computed result and rightly runs for a deleted
    /// job.) The skipped row is retired rather than left `Pending`, so a later
    /// restore cannot resurrect the stale fire.
    async fn recover_pending(&self) {
        let pending = match self
            .store
            .list_executions_by_status(ExecutionStatus::Pending)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                error!(error = %e, "failed to query pending executions for recovery");
                return;
            }
        };

        for exec in pending {
            match self.store.get(&exec.job_id).await {
                Ok(Some(job)) if !job.is_deleted() => {}
                Ok(_) => {
                    info!(
                        execution_id = %exec.id,
                        job_id = %exec.job_id,
                        "dropping pending cron execution: its job is no longer live"
                    );
                    if let Err(e) = self
                        .store
                        .update_execution_status(&exec.id, ExecutionStatus::Dispatched)
                        .await
                    {
                        error!(execution_id = %exec.id, error = %e, "failed to retire dropped execution");
                    }
                    continue;
                }
                Err(e) => {
                    error!(execution_id = %exec.id, error = %e, "failed to resolve the job of a pending execution; leaving it for the next restart");
                    continue;
                }
            }

            info!(
                execution_id = %exec.id,
                job_id = %exec.job_id,
                "re-dispatching pending cron execution after restart"
            );

            if let Err(e) = self
                .trigger_tx
                .send(CronTriggerEvent::for_execution(&exec))
                .await
            {
                error!(execution_id = %exec.id, error = %e, "failed to re-dispatch pending execution");
                continue;
            }

            if let Err(e) = self
                .store
                .update_execution_status(&exec.id, ExecutionStatus::Dispatched)
                .await
            {
                error!(execution_id = %exec.id, error = %e, "failed to mark recovered execution as dispatched");
            }
        }
    }

    async fn tick(&self) {
        let now = Utc::now();
        let due = match self.store.list_due(now.timestamp_micros()).await {
            Ok(jobs) => jobs,
            Err(e) => {
                error!(error = %e, "failed to query due cron jobs");
                return;
            }
        };

        for mut job in due {
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
            let execution = CronExecution::pending(&job, scheduled_fire_time, now);
            match self
                .store
                .record_execution(&execution)
                .await
                .map_err(CronError::from)
            {
                Ok(()) => {}
                Err(CronError::AlreadyDispatched(key)) => {
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
            if let Err(e) = self
                .trigger_tx
                .send(CronTriggerEvent::for_execution(&execution))
                .await
            {
                error!(job_id = %execution.job_id, error = %e, "failed to send cron trigger");
                // Execution stays Pending — will be recovered on next restart
                continue;
            }

            // Phase 4: Mark as Dispatched
            if let Err(e) = self
                .store
                .update_execution_status(&execution.id, ExecutionStatus::Dispatched)
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
    use crate::test_support::InMemoryCronStore;

    fn make_scheduler(
        store: InMemoryCronStore,
    ) -> (CronScheduler, mpsc::Receiver<CronTriggerEvent>) {
        let (tx, rx) = mpsc::channel(64);
        let scheduler = CronScheduler::new(Arc::new(store), tx, Arc::new(NeverShutdown));
        (scheduler, rx)
    }

    /// A `NewCronJob` spec with test defaults; override fields per test.
    fn spec(user_id: &str, schedule: CronSchedule, prompt: &str) -> NewCronJob {
        NewCronJob {
            user_id: user_id.to_string(),
            channel: ChannelType::tui(),
            title: "test job".to_string(),
            schedule,
            prompt: prompt.to_string(),
            timezone: "UTC".to_string(),
            origin_session_id: None,
        }
    }

    /// Helper: create a recurring prompt cron job.
    async fn create_prompt_cron(
        scheduler: &CronScheduler,
        user_id: &str,
        expr: &str,
        prompt: &str,
    ) -> CronJob {
        scheduler
            .create_job(spec(user_id, CronSchedule::cron(expr), prompt))
            .await
            .unwrap()
    }

    /// Helper: rewrite a job's `next_trigger_at` to a past instant so the
    /// next `tick()` treats it as due.
    async fn backdate_next_trigger(scheduler: &CronScheduler, job_id: &str) {
        let mut job = scheduler.store.get(job_id).await.unwrap().unwrap();
        job.next_trigger_at = Some(Utc::now() - chrono::Duration::seconds(10));
        scheduler.store.save(&job).await.unwrap();
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
            .create_job(spec("u1", CronSchedule::at(fire_at), "later"))
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
            .create_job(NewCronJob {
                timezone: "Asia/Shanghai".to_string(),
                ..spec("u1", CronSchedule::at(past), "too late")
            })
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
            .create_job(spec("u1", CronSchedule::cron("not a cron"), "test"))
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
            .create_job(NewCronJob {
                timezone: "Asia/Shanghai".to_string(),
                ..spec("u1", CronSchedule::cron("0 9 * * *"), "morning")
            })
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
            .create_job(NewCronJob {
                timezone: "Mars/Olympus_Mons".to_string(),
                ..spec("u1", CronSchedule::cron("0 9 * * *"), "x")
            })
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
            .create_job(spec("u1", CronSchedule::at(fire_at), "later"))
            .await
            .unwrap();
        scheduler.disable_job(&job.id).await.unwrap();

        // Simulate passage of time past the fire point by rewriting the
        // job's schedule to an `At` in the past.
        let mut stored = scheduler.store.get(&job.id).await.unwrap().unwrap();
        stored.schedule = CronSchedule::at(Utc::now() - chrono::Duration::seconds(10));
        scheduler.store.save(&stored).await.unwrap();

        let err = scheduler.enable_job(&job.id).await.unwrap_err();
        assert!(matches!(err, CronError::InvalidSchedule(_)));
    }

    #[tokio::test]
    async fn tick_fires_due_jobs() {
        let store = InMemoryCronStore::new();
        let (scheduler, mut rx) = make_scheduler(store);

        let job = create_prompt_cron(&scheduler, "u1", "* * * * *", "every minute").await;
        backdate_next_trigger(&scheduler, &job.id).await;

        scheduler.tick().await;

        let event = rx.try_recv().unwrap();
        assert_eq!(event.job_id, job.id);
        assert_eq!(event.prompt, "every minute");

        // Verify next_trigger_at was advanced
        let updated = scheduler.store.get(&job.id).await.unwrap().unwrap();
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
        backdate_next_trigger(&scheduler, &job.id).await;

        scheduler.tick().await;
        assert!(rx.try_recv().is_err());
    }

    /// The whole point of the recycle bin: a deleted job disappears from the
    /// user's list and — the part that must never regress — the tick loop
    /// refuses to fire it, however overdue it looks. The row itself survives.
    #[tokio::test]
    async fn deleted_job_leaves_the_listings_and_never_fires() {
        let (scheduler, mut rx) = make_scheduler(InMemoryCronStore::new());
        let job = create_prompt_cron(&scheduler, "u1", "* * * * *", "test").await;

        scheduler.delete_job(&job.id).await.unwrap();
        backdate_next_trigger(&scheduler, &job.id).await;
        scheduler.tick().await;

        assert!(rx.try_recv().is_err(), "a deleted job must not fire");
        assert!(scheduler.list_executions(&job.id).await.unwrap().is_empty());
        assert!(scheduler.list_jobs("u1").await.unwrap().is_empty());
        assert!(scheduler.list_all_jobs().await.unwrap().is_empty());

        let stored = scheduler
            .get_job(&job.id)
            .await
            .unwrap()
            .expect("a deleted job still resolves by id");
        assert!(stored.is_deleted());
        assert_eq!(
            stored.status,
            CronStatus::Enabled,
            "deletion leaves status alone"
        );

        let bin = scheduler.list_deleted_jobs().await.unwrap();
        assert_eq!(bin.len(), 1);
        assert_eq!(bin[0].id, job.id);
    }

    /// A job deleted with an overdue slot and restored later must come back
    /// with a fresh future slot — not fire on restore, not back-fill the slots
    /// it missed.
    #[tokio::test]
    async fn restore_recomputes_the_next_trigger_from_now() {
        let (scheduler, mut rx) = make_scheduler(InMemoryCronStore::new());
        let job = create_prompt_cron(&scheduler, "u1", "* * * * *", "test").await;

        scheduler.delete_job(&job.id).await.unwrap();
        backdate_next_trigger(&scheduler, &job.id).await;

        scheduler.restore_job(&job.id).await.unwrap();

        let restored = scheduler.get_job(&job.id).await.unwrap().unwrap();
        assert!(!restored.is_deleted());
        assert_eq!(restored.status, CronStatus::Enabled);
        let next = restored
            .next_trigger_at
            .expect("restored enabled job fires");
        assert!(next > Utc::now(), "restored into a past slot: {next}");

        scheduler.tick().await;
        assert!(
            rx.try_recv().is_err(),
            "a restore must not fire the slots missed while deleted"
        );
        assert_eq!(scheduler.list_jobs("u1").await.unwrap().len(), 1);
        assert!(scheduler.list_deleted_jobs().await.unwrap().is_empty());
    }

    /// A one-shot whose instant passed while it sat in the bin has no fire time
    /// left: it comes back disabled rather than enabled-with-a-past-slot.
    #[tokio::test]
    async fn restore_of_an_expired_one_shot_comes_back_disabled() {
        let (scheduler, mut rx) = make_scheduler(InMemoryCronStore::new());
        let job = scheduler
            .create_job(spec(
                "u1",
                CronSchedule::at(Utc::now() + chrono::Duration::seconds(30)),
                "later",
            ))
            .await
            .unwrap();
        scheduler.delete_job(&job.id).await.unwrap();

        // Its moment passes while it is in the bin.
        let mut stored = scheduler.store.get(&job.id).await.unwrap().unwrap();
        stored.schedule = CronSchedule::at(Utc::now() - chrono::Duration::seconds(10));
        scheduler.store.save(&stored).await.unwrap();

        scheduler.restore_job(&job.id).await.unwrap();

        let restored = scheduler.get_job(&job.id).await.unwrap().unwrap();
        assert!(!restored.is_deleted());
        assert_eq!(restored.status, CronStatus::Disabled);
        assert!(restored.next_trigger_at.is_none());

        scheduler.tick().await;
        assert!(rx.try_recv().is_err());
    }

    /// A one-shot that already fired keeps `Executed` through the round trip —
    /// status is orthogonal to deletion.
    #[tokio::test]
    async fn restore_keeps_an_executed_one_shot_executed() {
        let (scheduler, mut rx) = make_scheduler(InMemoryCronStore::new());
        let job = scheduler
            .create_job(spec(
                "u1",
                CronSchedule::at(Utc::now() + chrono::Duration::seconds(30)),
                "run once",
            ))
            .await
            .unwrap();
        backdate_next_trigger(&scheduler, &job.id).await;
        scheduler.tick().await;
        assert!(rx.try_recv().is_ok());

        scheduler.delete_job(&job.id).await.unwrap();
        scheduler.restore_job(&job.id).await.unwrap();

        let restored = scheduler.get_job(&job.id).await.unwrap().unwrap();
        assert_eq!(restored.status, CronStatus::Executed);
        assert!(restored.next_trigger_at.is_none());
        assert!(!restored.is_deleted());
    }

    #[tokio::test]
    async fn restore_of_a_live_job_leaves_its_schedule_alone() {
        let (scheduler, _rx) = make_scheduler(InMemoryCronStore::new());
        let job = create_prompt_cron(&scheduler, "u1", "0 9 * * *", "test").await;

        scheduler.restore_job(&job.id).await.unwrap();

        let unchanged = scheduler.get_job(&job.id).await.unwrap().unwrap();
        assert_eq!(unchanged.next_trigger_at, job.next_trigger_at);
        assert_eq!(unchanged.status, CronStatus::Enabled);
    }

    #[tokio::test]
    async fn restore_errors_for_missing_job() {
        let (scheduler, _rx) = make_scheduler(InMemoryCronStore::new());
        let err = scheduler.restore_job("ghost").await.unwrap_err();
        assert!(matches!(err, CronError::NotFound(_)));
    }

    /// A restored job resumes its cadence — it fires once when its recomputed
    /// slot arrives, and does not make up the slots it slept through.
    #[tokio::test]
    async fn restore_fires_once_at_the_next_slot_not_a_catch_up_burst() {
        let (scheduler, mut rx) = make_scheduler(InMemoryCronStore::new());
        let job = create_prompt_cron(&scheduler, "u1", "* * * * *", "test").await;

        scheduler.delete_job(&job.id).await.unwrap();
        backdate_next_trigger(&scheduler, &job.id).await;
        scheduler.restore_job(&job.id).await.unwrap();

        // The next slot arrives.
        backdate_next_trigger(&scheduler, &job.id).await;
        scheduler.tick().await;
        assert!(
            rx.try_recv().is_ok(),
            "the restored job resumed its cadence"
        );
        assert!(
            rx.try_recv().is_err(),
            "the slots missed while deleted were made up in a burst",
        );

        // And the cadence continues from there rather than re-firing the slot.
        scheduler.tick().await;
        assert!(rx.try_recv().is_err());
        let after = scheduler.get_job(&job.id).await.unwrap().unwrap();
        assert!(after.next_trigger_at.is_some_and(|t| t > Utc::now()));
        assert_eq!(scheduler.list_executions(&job.id).await.unwrap().len(), 1);
    }

    /// Pause takes the job out of the firing set; resume puts it back with a
    /// slot in the future, never the stale one it carried before.
    #[tokio::test]
    async fn pause_clears_the_slot_and_resume_recomputes_it_forward() {
        let (scheduler, mut rx) = make_scheduler(InMemoryCronStore::new());
        let job = create_prompt_cron(&scheduler, "u1", "* * * * *", "test").await;
        backdate_next_trigger(&scheduler, &job.id).await;

        scheduler.disable_job(&job.id).await.unwrap();
        let paused = scheduler.get_job(&job.id).await.unwrap().unwrap();
        assert_eq!(paused.status, CronStatus::Disabled);
        assert!(paused.next_trigger_at.is_none());
        scheduler.tick().await;
        assert!(rx.try_recv().is_err(), "a paused job must not fire");

        scheduler.enable_job(&job.id).await.unwrap();
        let resumed = scheduler.get_job(&job.id).await.unwrap().unwrap();
        assert_eq!(resumed.status, CronStatus::Enabled);
        let next = resumed.next_trigger_at.expect("a resumed job is scheduled");
        assert!(next > Utc::now(), "resumed into a past slot: {next}");

        scheduler.tick().await;
        assert!(
            rx.try_recv().is_err(),
            "resume back-filled the slots missed while paused",
        );
    }

    /// Manually firing a job is the one path that reaches a job by id rather
    /// than through a listing, so it is the one path where a deleted job could
    /// still fire. It must not.
    #[tokio::test]
    async fn trigger_now_refuses_a_deleted_job() {
        let (scheduler, mut rx) = make_scheduler(InMemoryCronStore::new());
        let job = create_prompt_cron(&scheduler, "u1", "0 9 * * *", "test").await;
        scheduler.delete_job(&job.id).await.unwrap();

        let err = scheduler.trigger_now(&job.id).await.unwrap_err();
        assert!(matches!(err, CronError::NotFound(_)), "{err:?}");

        assert!(rx.try_recv().is_err(), "a deleted job fired on demand");
        assert!(
            scheduler.list_executions(&job.id).await.unwrap().is_empty(),
            "a deleted job recorded an execution",
        );
    }

    /// Resuming reaches a job by id too, and `get` resolves deleted jobs. A
    /// job in the bin reads as absent to pause/resume: reporting success would
    /// promise a fire that `list_due` can never produce, and the LLM tool would
    /// tell the user their job is back on. Only `restore_job` brings a job back.
    #[tokio::test]
    async fn resuming_a_deleted_job_is_not_found() {
        let (scheduler, mut rx) = make_scheduler(InMemoryCronStore::new());
        let job = create_prompt_cron(&scheduler, "u1", "* * * * *", "test").await;
        scheduler.delete_job(&job.id).await.unwrap();

        let err = scheduler.enable_job(&job.id).await.unwrap_err();
        assert!(matches!(err, CronError::NotFound(_)), "{err:?}");

        backdate_next_trigger(&scheduler, &job.id).await;
        scheduler.tick().await;

        assert!(rx.try_recv().is_err(), "a resumed-while-deleted job fired");
        assert!(
            scheduler
                .get_job(&job.id)
                .await
                .unwrap()
                .is_some_and(|j| j.is_deleted()),
            "resume pulled the job out of the recycle bin",
        );
        assert!(scheduler.list_all_jobs().await.unwrap().is_empty());
    }

    /// Pausing a job in the bin would rewrite the status `restore_job` promises
    /// to bring it back with — an enabled job would silently return paused.
    #[tokio::test]
    async fn pausing_a_deleted_job_is_not_found_and_leaves_the_restore_status_alone() {
        let (scheduler, _rx) = make_scheduler(InMemoryCronStore::new());
        let job = create_prompt_cron(&scheduler, "u1", "* * * * *", "test").await;
        scheduler.delete_job(&job.id).await.unwrap();

        let err = scheduler.disable_job(&job.id).await.unwrap_err();
        assert!(matches!(err, CronError::NotFound(_)), "{err:?}");

        scheduler.restore_job(&job.id).await.unwrap();
        let restored = scheduler.get_job(&job.id).await.unwrap().unwrap();
        assert_eq!(restored.status, CronStatus::Enabled);
        assert!(restored.next_trigger_at.is_some());
    }

    /// The tick loop reads a due job, works, then writes it back. A delete
    /// landing inside that window must survive the write-back — otherwise the
    /// job the user just deleted is live again, and still firing.
    #[tokio::test]
    async fn a_delete_racing_the_tick_loops_write_back_is_not_undone() {
        let (scheduler, mut rx) = make_scheduler(InMemoryCronStore::new());
        let job = create_prompt_cron(&scheduler, "u1", "* * * * *", "test").await;
        backdate_next_trigger(&scheduler, &job.id).await;

        // The snapshot the tick loop is holding, taken while the job was live.
        let in_flight = scheduler.store.get(&job.id).await.unwrap().unwrap();
        assert!(!in_flight.is_deleted());

        scheduler.delete_job(&job.id).await.unwrap();

        // The tick loop advances its snapshot to the next slot (`advance_recurring`).
        scheduler.store.save(&in_flight).await.unwrap();

        let after = scheduler.get_job(&job.id).await.unwrap().unwrap();
        assert!(
            after.is_deleted(),
            "the write-back resurrected a deleted job"
        );
        scheduler.tick().await;
        assert!(rx.try_recv().is_err(), "a resurrected job fired");
        assert!(scheduler.list_all_jobs().await.unwrap().is_empty());
    }

    /// Boot recovery re-dispatches `Pending` executions — and a `Pending`
    /// execution has never run, so re-dispatching it *fires* the job. A job
    /// the user deleted in the crash window must not come back to life through
    /// this door, and its orphaned row must not be left to try again next boot.
    #[tokio::test]
    async fn a_deleted_jobs_pending_execution_is_not_re_dispatched() {
        let (scheduler, mut rx) = make_scheduler(InMemoryCronStore::new());
        let job = scheduler
            .create_job(spec(
                "u1",
                CronSchedule::at(Utc::now() + chrono::Duration::seconds(30)),
                "remind me",
            ))
            .await
            .unwrap();
        let stored = scheduler.store.get(&job.id).await.unwrap().unwrap();
        let exec = CronExecution::pending(&stored, Utc::now(), Utc::now());
        scheduler.store.record_execution(&exec).await.unwrap();

        scheduler.delete_job(&job.id).await.unwrap();
        scheduler.recover_pending().await;

        assert!(
            rx.try_recv().is_err(),
            "a deleted job fired on boot recovery"
        );

        scheduler.restore_job(&job.id).await.unwrap();
        scheduler.recover_pending().await;
        assert!(
            rx.try_recv().is_err(),
            "restoring the job resurrected the stale pending fire",
        );
    }

    /// A live job's pending execution is the case boot recovery exists for:
    /// the fire was recorded but the process died before it was dispatched.
    #[tokio::test]
    async fn a_live_jobs_pending_execution_is_re_dispatched() {
        let (scheduler, mut rx) = make_scheduler(InMemoryCronStore::new());
        let job = scheduler
            .create_job(spec(
                "u1",
                CronSchedule::at(Utc::now() + chrono::Duration::seconds(30)),
                "remind me",
            ))
            .await
            .unwrap();
        let stored = scheduler.store.get(&job.id).await.unwrap().unwrap();
        let exec = CronExecution::pending(&stored, Utc::now(), Utc::now());
        scheduler.store.record_execution(&exec).await.unwrap();

        scheduler.recover_pending().await;

        let event = rx
            .try_recv()
            .expect("the recorded fire was never dispatched");
        assert_eq!(event.job_id, job.id);
        assert_eq!(event.prompt, "remind me");
    }

    /// The tick loop reads a due job, fires it, then writes the advanced
    /// schedule back. A pause landing inside that window must survive the
    /// write-back — otherwise the stale snapshot re-arms the job and it keeps
    /// firing, with the user's only stop control silently undone.
    #[tokio::test]
    async fn a_pause_racing_the_tick_loops_write_back_is_not_undone() {
        let (scheduler, _rx) = make_scheduler(InMemoryCronStore::new());
        let job = create_prompt_cron(&scheduler, "u1", "* * * * *", "test").await;
        backdate_next_trigger(&scheduler, &job.id).await;

        // The snapshot the tick loop is holding, taken while the job was enabled.
        let mut in_flight = scheduler.store.get(&job.id).await.unwrap().unwrap();

        scheduler.disable_job(&job.id).await.unwrap();

        scheduler
            .advance_recurring(&mut in_flight, Utc::now())
            .await;

        let after = scheduler.get_job(&job.id).await.unwrap().unwrap();
        assert_eq!(
            after.status,
            CronStatus::Disabled,
            "the tick write-back un-paused the job",
        );
        assert!(
            after.next_trigger_at.is_none(),
            "the tick write-back re-armed a paused job",
        );
    }

    #[tokio::test]
    async fn at_job_marked_executed_after_firing() {
        let store = InMemoryCronStore::new();
        let (scheduler, mut rx) = make_scheduler(store);

        let fire_at = Utc::now() + chrono::Duration::seconds(30);
        let job = scheduler
            .create_job(spec("u1", CronSchedule::at(fire_at), "run once"))
            .await
            .unwrap();
        let job_id = job.id.clone();

        backdate_next_trigger(&scheduler, &job_id).await;
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

    #[tokio::test]
    async fn tick_idempotent_does_not_double_fire() {
        let store = InMemoryCronStore::new();
        let (scheduler, mut rx) = make_scheduler(store);

        let job = create_prompt_cron(&scheduler, "u1", "* * * * *", "dedup test").await;
        backdate_next_trigger(&scheduler, &job.id).await;

        // First tick fires
        scheduler.tick().await;
        assert!(rx.try_recv().is_ok());

        // Rewind `next_trigger_at` to the same slot as the recorded execution
        // to simulate a re-trigger attempt for an already-recorded slot.
        let execs = scheduler.list_executions(&job.id).await.unwrap();
        let mut job = scheduler.store.get(&job.id).await.unwrap().unwrap();
        job.next_trigger_at = Some(execs[0].scheduled_fire_time);
        scheduler.store.save(&job).await.unwrap();

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
        let mut job = CronJob {
            id: "cj-1".to_string(),
            user_id: "u1".to_string(),
            channel: ChannelType::tui(),
            title: "recovered".to_string(),
            schedule: CronSchedule::cron("* * * * *"),
            prompt: "recover me".to_string(),
            timezone: "UTC".to_string(),
            status: CronStatus::Enabled,
            last_triggered_at: None,
            next_trigger_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            origin_session_id: None,
            deleted_at: None,
        };
        job.next_trigger_at = Some(Utc::now());
        store.create(&job).await.unwrap();
        let mut exec = CronExecution::pending(&job, Utc::now(), Utc::now());
        exec.id = "ce-pending".to_string();
        store.record_execution(&exec).await.unwrap();

        let (scheduler, mut rx) = make_scheduler(store);
        scheduler.recover_pending().await;

        // The pending execution was re-dispatched
        let event = rx.try_recv().unwrap();
        assert_eq!(event.job_id, "cj-1");
        assert_eq!(event.prompt, "recover me");

        // Status updated to dispatched
        let execs = scheduler
            .store
            .list_executions_by_status(ExecutionStatus::Dispatched)
            .await
            .unwrap();
        assert_eq!(execs.len(), 1);
        assert_eq!(execs[0].id, "ce-pending");

        // No pending left
        let pending = scheduler
            .store
            .list_executions_by_status(ExecutionStatus::Pending)
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
            .create_job(spec("u1", CronSchedule::at(fire_at), "manual one-shot"))
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

    /// The fire event carries everything the agent layer needs to run and
    /// deliver the fire — including the execution id it stamps the outcome
    /// onto, and whether the job is one-shot (its result belongs in the origin
    /// conversation rather than its own).
    #[tokio::test]
    async fn trigger_event_carries_execution_title_and_one_shot() {
        let (scheduler, mut rx) = make_scheduler(InMemoryCronStore::new());
        let fire_at = Utc::now() + chrono::Duration::minutes(5);
        let job = scheduler
            .create_job(NewCronJob {
                title: "晚饭提醒".to_string(),
                ..spec("u1", CronSchedule::at(fire_at), "Remind the user to eat")
            })
            .await
            .unwrap();

        let exec = scheduler.trigger_now(&job.id).await.unwrap();
        let event = rx.try_recv().unwrap();
        assert_eq!(event.execution_id, exec.id);
        assert_eq!(event.title, "晚饭提醒");
        assert!(event.one_shot);

        // A recurring job's fire is not one-shot and titles its own conversation.
        let recurring = create_prompt_cron(&scheduler, "u1", "0 9 * * *", "news").await;
        scheduler.trigger_now(&recurring.id).await.unwrap();
        let event = rx.try_recv().unwrap();
        assert!(!event.one_shot);
        assert_eq!(event.title, "test job");
    }

    /// A title-less legacy job still names its fire — the event falls back to a
    /// truncated prompt rather than an empty string.
    #[tokio::test]
    async fn trigger_event_titles_a_legacy_job_from_its_prompt() {
        let (scheduler, mut rx) = make_scheduler(InMemoryCronStore::new());
        let job = scheduler
            .create_job(NewCronJob {
                title: String::new(),
                ..spec("u1", CronSchedule::cron("0 9 * * *"), "Summarise the news")
            })
            .await
            .unwrap();
        scheduler.trigger_now(&job.id).await.unwrap();
        assert_eq!(rx.try_recv().unwrap().title, "Summarise the news");
    }

    /// The delivery ledger: a completed fire awaits delivery until it is
    /// resolved, and only one-shot executions are ever re-driven (a recurring
    /// fire's result lives in its own conversation — there is nothing to
    /// deliver elsewhere).
    #[tokio::test]
    async fn awaiting_delivery_scan_covers_one_shots_until_resolved() {
        let (scheduler, _rx) = make_scheduler(InMemoryCronStore::new());

        let one_shot = scheduler
            .create_job(spec(
                "u1",
                CronSchedule::at(Utc::now() + chrono::Duration::minutes(5)),
                "once",
            ))
            .await
            .unwrap();
        let recurring = create_prompt_cron(&scheduler, "u1", "0 9 * * *", "daily").await;
        let one_shot_exec = scheduler.trigger_now(&one_shot.id).await.unwrap();
        let recurring_exec = scheduler.trigger_now(&recurring.id).await.unwrap();

        // Neither has completed yet.
        assert!(
            scheduler
                .list_executions_awaiting_delivery()
                .await
                .unwrap()
                .is_empty()
        );

        for exec_id in [&one_shot_exec.id, &recurring_exec.id] {
            scheduler
                .record_execution_completion(
                    exec_id,
                    ExecutionCompletion {
                        fire_session_id: "cron-fire".into(),
                        outcome: baybo_model::ExecutionOutcome::Success,
                        reply_ordinal: Some(3),
                        completed_at: Utc::now(),
                    },
                )
                .await
                .unwrap();
        }

        let awaiting = scheduler.list_executions_awaiting_delivery().await.unwrap();
        assert_eq!(
            awaiting.len(),
            1,
            "only the one-shot's result is delivered elsewhere"
        );
        assert_eq!(awaiting[0].id, one_shot_exec.id);

        scheduler
            .mark_execution_notified(&one_shot_exec.id, Utc::now())
            .await
            .unwrap();
        assert!(
            scheduler
                .list_executions_awaiting_delivery()
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn trigger_carries_origin_session_id_through_event() {
        let (scheduler, mut rx) = make_scheduler(InMemoryCronStore::new());
        let future = Utc::now() + chrono::Duration::hours(1);
        let job = scheduler
            .create_job(NewCronJob {
                origin_session_id: Some("sess-creator".into()),
                ..spec("u1", CronSchedule::at(future), "lineage carries")
            })
            .await
            .unwrap();
        scheduler.trigger_now(&job.id).await.unwrap();
        let event = rx.try_recv().expect("trigger event must land");
        assert_eq!(
            event.origin_session_id.as_ref().map(|s| s.as_str()),
            Some("sess-creator"),
        );
    }
}
