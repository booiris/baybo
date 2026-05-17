//! `CronStore` — persistence interface for cron jobs and executions.
//!
//! The trait talks in domain types (`CronJob`, `CronExecution`,
//! `ExecutionStatus`) directly; the libsql implementation in
//! `aura-storage::libsql::cron` handles JSON round-tripping internally.
//! Previously the trait carried opaque `CronJobRow` / `CronExecutionRow`
//! shapes so `aura-storage` could stay independent of `aura-cron`; the
//! dep edge has since flipped to match the rest of the workspace (every
//! domain crate owns its own trait, storage depends on each domain).

use async_trait::async_trait;

use crate::error::CronError;
use crate::job::{CronExecution, CronJob, ExecutionStatus};

pub type Result<T> = std::result::Result<T, CronError>;

/// Persistence layer for cron job records and execution records.
#[async_trait]
pub trait CronStore: Send + Sync {
    // ── Job CRUD ──

    async fn create(&self, job: &CronJob) -> Result<()>;
    async fn get(&self, job_id: &str) -> Result<Option<CronJob>>;
    async fn save(&self, job: &CronJob) -> Result<()>;
    async fn delete(&self, job_id: &str) -> Result<()>;
    async fn list_by_user(&self, user_id: &str) -> Result<Vec<CronJob>>;
    /// Return every stored cron job regardless of user or status. Ordering is
    /// unspecified — callers sort as needed.
    async fn list_all(&self) -> Result<Vec<CronJob>>;
    async fn list_enabled(&self) -> Result<Vec<CronJob>>;
    /// Return all enabled jobs whose `next_trigger_at` is at or before
    /// `now_us` (Unix microseconds).
    async fn list_due(&self, now_us: i64) -> Result<Vec<CronJob>>;

    // ── Execution records ──

    /// Persist a fresh execution. Returns
    /// `Err(CronError::AlreadyDispatched(_))` when two scheduler
    /// instances race on the same `(job_id, scheduled_fire_time)`
    /// slot — the unique index rejected the loser's insert. The tick
    /// path treats this as benign and skips the duplicate dispatch.
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

    /// List all execution records with the given status.
    /// Used at startup to find `Pending` executions that need re-dispatch.
    async fn list_executions_by_status(
        &self,
        status: ExecutionStatus,
    ) -> Result<Vec<CronExecution>>;
}
