use async_trait::async_trait;
use baybo_model::{CronExecution, CronJob, ExecutionOutcome, ExecutionStatus, SessionId};

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Persistence interface for cron jobs and executions. Implemented by
/// `baybo_storage::sqlite::SqliteCronStore`. The trait talks in the domain
/// types (`CronJob`, `CronExecution`, `ExecutionStatus`) from
/// `baybo-model`; the libsql impl handles JSON round-tripping internally.
#[async_trait]
pub trait CronStore: Send + Sync {
    // ── Job CRUD ──
    async fn create(&self, job: &CronJob) -> Result<()>;
    async fn get(&self, job_id: &str) -> Result<Option<CronJob>>;
    async fn save(&self, job: &CronJob) -> Result<()>;
    async fn delete(&self, job_id: &str) -> Result<()>;
    async fn list_by_user(&self, user_id: &str) -> Result<Vec<CronJob>>;
    /// Return every stored cron job regardless of user or status. Ordering
    /// is unspecified — callers sort as needed.
    async fn list_all(&self) -> Result<Vec<CronJob>>;
    async fn list_enabled(&self) -> Result<Vec<CronJob>>;
    /// Return all enabled jobs whose `next_trigger_at` is at or before
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
