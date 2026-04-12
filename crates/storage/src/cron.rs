use async_trait::async_trait;

/// Storage-level error for cron persistence operations.
#[derive(Debug)]
pub enum CronStoreError {
    /// The requested record was not found.
    NotFound(String),
    /// An internal storage error occurred.
    Internal(String),
}

impl std::fmt::Display for CronStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "not found: {id}"),
            Self::Internal(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for CronStoreError {}

pub type Result<T> = std::result::Result<T, CronStoreError>;

/// A raw persistence record for a cron job.
///
/// Fields mirror the DB columns. The `data` column holds an opaque JSON blob
/// owned by the caller; the remaining columns are extracted for indexed queries.
#[derive(Debug, Clone)]
pub struct CronJobRow {
    pub id: String,
    pub user_id: String,
    pub status: String,
    pub next_trigger_at: String,
    pub data: String,
}

/// A raw persistence record for a cron execution.
#[derive(Debug, Clone)]
pub struct CronExecutionRow {
    pub id: String,
    pub job_id: String,
    pub user_id: String,
    /// The schedule slot that was due (`next_trigger_at` at fire time).
    pub scheduled_fire_time: String,
    pub triggered_at: String,
    /// Execution lifecycle status: `"pending"` or `"dispatched"`.
    pub status: String,
    pub data: String,
}

/// Persistence layer for cron job records and execution records.
#[async_trait]
pub trait CronStore: Send + Sync {
    // ── Job CRUD ──

    async fn create(&self, row: &CronJobRow) -> Result<()>;
    async fn get(&self, job_id: &str) -> Result<Option<CronJobRow>>;
    async fn save(&self, row: &CronJobRow) -> Result<()>;
    async fn delete(&self, job_id: &str) -> Result<()>;
    async fn list_by_user(&self, user_id: &str) -> Result<Vec<CronJobRow>>;
    /// Return every stored cron job regardless of user or status. Ordering is
    /// unspecified — callers sort as needed.
    async fn list_all(&self) -> Result<Vec<CronJobRow>>;
    async fn list_enabled(&self) -> Result<Vec<CronJobRow>>;
    /// Return all enabled job rows whose `next_trigger_at` is at or before `now`.
    async fn list_due(&self, now: &str) -> Result<Vec<CronJobRow>>;

    // ── Execution records ──

    async fn record_execution(&self, row: &CronExecutionRow) -> Result<()>;
    async fn list_executions_by_job(&self, job_id: &str) -> Result<Vec<CronExecutionRow>>;
    async fn list_executions_by_user(&self, user_id: &str) -> Result<Vec<CronExecutionRow>>;

    /// Check if an execution already exists for this (job_id, scheduled_fire_time) pair.
    /// Used for idempotent tick: prevents duplicate triggers for the same schedule slot.
    async fn has_execution_for_schedule(
        &self,
        job_id: &str,
        scheduled_fire_time: &str,
    ) -> Result<bool>;

    /// Update the status of an execution record (e.g. `"pending"` → `"dispatched"`).
    async fn update_execution_status(&self, execution_id: &str, status: &str) -> Result<()>;

    /// List all execution records with the given status.
    /// Used at startup to find `"pending"` executions that need re-dispatch.
    async fn list_executions_by_status(&self, status: &str) -> Result<Vec<CronExecutionRow>>;
}
