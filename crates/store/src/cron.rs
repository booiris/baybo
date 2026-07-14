use async_trait::async_trait;
use baybo_model::{CronExecution, CronJob, ExecutionOutcome, ExecutionStatus, SessionId};

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Persistence interface for cron jobs and executions. Implemented by
/// `baybo_storage::sqlite::SqliteCronStore`. The trait talks in the domain
/// types (`CronJob`, `CronExecution`, `ExecutionStatus`) from
/// `baybo-model`; the sqlite impl handles JSON round-tripping internally.
///
/// Deletion is a recycle bin: [`CronStore::delete`] stamps
/// `CronJob::deleted_at` and the row lives on. Every listing below returns
/// **live jobs only** — the filter is applied in SQL, so a deleted job can
/// never reach the tick loop or a user's list. [`CronStore::get`] is the one
/// exception: it resolves a deleted job by id, which is what keeps its
/// execution rows and the conversations they opened pointing at a real job.
#[async_trait]
pub trait CronStore: Send + Sync {
    // ── Job CRUD ──
    async fn create(&self, job: &CronJob) -> Result<()>;
    /// Fetch a job by id, deleted or not.
    async fn get(&self, job_id: &str) -> Result<Option<CronJob>>;
    /// Persist a job's schedule and lifecycle — everything *except* its
    /// recycle-bin state, which only [`CronStore::delete`] and
    /// [`CronStore::restore`] may move. The tick loop reads a job, works, then
    /// writes it back; a `deleted_at` that this write could carry would let
    /// that stale snapshot undo a deletion that landed in between, putting a
    /// job the user deleted back on the schedule.
    async fn save(&self, job: &CronJob) -> Result<()>;
    /// Write back a recurring job's advanced schedule after a fire — but only
    /// while the row is still the enabled, live job the tick loop read. A pause
    /// or a delete landing inside the fire window must survive this write-back;
    /// an unconditional `save` would re-arm, from a stale snapshot, a job the
    /// user just stopped. Returns false when the row moved and the write was
    /// dropped.
    async fn save_if_still_enabled(&self, job: &CronJob) -> Result<bool>;
    /// Move a job to the recycle bin by stamping `deleted_at`. Leaves
    /// `status` untouched. Idempotent: a job already in the bin keeps the
    /// deletion time it went in with.
    async fn delete(&self, job_id: &str) -> Result<()>;
    /// Bring a job back from the recycle bin by clearing `deleted_at`.
    /// Leaves `status` and `next_trigger_at` untouched — the caller is
    /// responsible for having already written a schedule that is safe to
    /// publish (see `CronScheduler::restore_job`).
    async fn restore(&self, job_id: &str) -> Result<()>;
    async fn list_by_user(&self, user_id: &str) -> Result<Vec<CronJob>>;
    /// Return every live cron job regardless of user or status. Ordering
    /// is unspecified — callers sort as needed.
    async fn list_all(&self) -> Result<Vec<CronJob>>;
    async fn list_enabled(&self) -> Result<Vec<CronJob>>;
    /// The recycle bin: every soft-deleted job, most recently deleted first.
    async fn list_deleted(&self) -> Result<Vec<CronJob>>;
    /// Return all enabled, live jobs whose `next_trigger_at` is at or before
    /// `now_us` (Unix microseconds).
    async fn list_due(&self, now_us: i64) -> Result<Vec<CronJob>>;

    // ── Execution records ──

    /// Persist a fresh execution. Returns [`StorageError::Conflict`] when
    /// two scheduler instances race on the same `(job_id,
    /// scheduled_fire_time)` slot — the unique index rejected the loser's
    /// insert. The tick path treats this as benign and skips the
    /// duplicate dispatch.
    async fn record_execution(&self, exec: &CronExecution) -> Result<()>;
    async fn list_executions_by_job(&self, job_id: &str) -> Result<Vec<CronExecution>>;
    async fn list_executions_by_user(&self, user_id: &str) -> Result<Vec<CronExecution>>;

    /// Check if an execution already exists for this (job_id,
    /// scheduled_fire_time) pair. Used for idempotent tick: prevents
    /// duplicate triggers for the same schedule slot.
    async fn has_execution_for_schedule(
        &self,
        job_id: &str,
        scheduled_fire_time_us: i64,
    ) -> Result<bool>;

    /// Transition an execution to a new status.
    async fn update_execution_status(
        &self,
        execution_id: &str,
        status: ExecutionStatus,
    ) -> Result<()>;

    /// List all execution records with the given status. Used at startup
    /// to find `Pending` executions that need re-dispatch.
    async fn list_executions_by_status(
        &self,
        status: ExecutionStatus,
    ) -> Result<Vec<CronExecution>>;

    // ── Delivery ledger (one-shot result → origin conversation) ──

    /// Stamp the fire's terminal state onto the execution: which session it
    /// ran in, how it ended, the ordinal of its reply row, and when. Written
    /// by the cron waiter **before** it routes the result, so a crash between
    /// here and the origin's append leaves a durable record for the boot
    /// re-drive to replay.
    async fn record_execution_completion(
        &self,
        execution_id: &str,
        completion: ExecutionCompletion,
    ) -> Result<()>;

    /// Mark this execution's delivery **resolved** — the result was appended
    /// to the origin conversation, or terminally dropped (no usable origin).
    /// Both stamp `notified_at`, so the re-drive scan converges either way.
    async fn mark_execution_notified(
        &self,
        execution_id: &str,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()>;

    /// Executions whose fire completed but whose result has not been
    /// delivered or dropped ([`CronExecution::awaits_delivery`]). Scanned at
    /// boot to re-drive deliveries lost to a crash.
    async fn list_executions_awaiting_delivery(&self) -> Result<Vec<CronExecution>>;
}

/// The fire's terminal state, stamped onto its [`CronExecution`] by the cron
/// waiter. Grouped into a struct so the store trait doesn't take five
/// positional arguments whose types (two `Option`s, a session id) are easy to
/// transpose at a call site.
#[derive(Debug, Clone)]
pub struct ExecutionCompletion {
    pub fire_session_id: SessionId,
    pub outcome: ExecutionOutcome,
    pub reply_ordinal: Option<i64>,
    pub completed_at: chrono::DateTime<chrono::Utc>,
}
