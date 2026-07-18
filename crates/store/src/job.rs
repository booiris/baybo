use async_trait::async_trait;
use baybo_model::{JobId, SessionId};
use chrono::{DateTime, Utc};

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Persistence row for a job.
///
/// The full domain `Job` (with its state machine and rich field types)
/// is serialized into `data`; the remaining fields are projected out so
/// the backend can index/filter without deserializing. `baybo-job` owns
/// the `Job` type and converts to/from this row at the persistence
/// boundary (`Job::to_row` / `Job::from_row`) — that keeps the job state
/// machine out of this leaf crate while still letting `JobStore` live
/// here alongside every other store contract.
#[derive(Debug, Clone)]
pub struct JobRow {
    pub id: JobId,
    pub session_id: SessionId,
    pub parent_job_id: Option<JobId>,
    /// The job's input kind rendered as its wire string. Denormalised
    /// for display only — never filtered in SQL (`from_row` rebuilds the
    /// whole `Job` from `data`).
    pub kind: String,
    /// `JobStatusKind` rendered as its snake_case string — drives the
    /// `list_by_status_kind` / `list_recoverable` filters.
    pub status_kind: String,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    /// The serialized `Job` aggregate. Opaque to this layer.
    pub data: String,
}

/// Persistence row for a job-state-transition audit record. `data` holds
/// the serialized `JobTransition`.
#[derive(Debug, Clone)]
pub struct JobTransitionRow {
    pub job_id: JobId,
    pub data: String,
}

/// Per-session job aggregates for list surfaces: how many jobs a
/// session has and the `status_kind` of its newest job (by
/// `created_at`). One grouped query replaces a `list_by_session`
/// fan-out across every session.
#[derive(Debug, Clone)]
pub struct SessionJobStats {
    pub session_id: SessionId,
    pub job_count: usize,
    /// `JobStatusKind` of the latest job, rendered as its snake_case
    /// wire string (same encoding as [`JobRow::status_kind`]).
    pub latest_status_kind: String,
}

/// Persistence backend for jobs and their transition audit log. Trades
/// in [`JobRow`] / [`JobTransitionRow`] rather than the rich `baybo-job`
/// types so the contract stays in this leaf crate.
#[async_trait]
pub trait JobStore: Send + Sync {
    async fn create(&self, job: &JobRow) -> Result<()>;
    async fn get(&self, job_id: &JobId) -> Result<Option<JobRow>>;
    /// Persist the mutable state of a job (status, timestamps, payload).
    async fn save(&self, job: &JobRow) -> Result<()>;
    async fn list_by_session(&self, session_id: &SessionId) -> Result<Vec<JobRow>>;
    /// Non-terminal jobs (`pending` / `in_progress` / `stuck`) for one session,
    /// scoped + status-filtered at the store. Lets callers (e.g. `/stop`) find
    /// a session's live jobs without loading a long-lived session's entire
    /// job history just to filter it down to the few in flight.
    async fn list_active_by_session(&self, session_id: &SessionId) -> Result<Vec<JobRow>>;
    /// Filter by the snake_case `JobStatusKind` wire string.
    async fn list_by_status_kind(&self, status_kind: &str) -> Result<Vec<JobRow>>;
    async fn list_children(&self, parent_job_id: &JobId) -> Result<Vec<JobRow>>;
    /// Return every stored job. Ordering unspecified — callers sort.
    async fn list_all(&self) -> Result<Vec<JobRow>>;
    /// One page of jobs, newest `created_at` first, optionally
    /// filtered by the snake_case `JobStatusKind` wire string, plus
    /// the total matching count. Ordering + paging live in SQL so a
    /// page never materialises the full table.
    async fn list_page(
        &self,
        status_kind: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<JobRow>, usize)>;
    /// Job aggregates grouped by session — one [`SessionJobStats`] per
    /// session that has at least one job. Ordering unspecified.
    async fn session_job_stats(&self) -> Result<Vec<SessionJobStats>>;
    /// Number of jobs with the given snake_case `JobStatusKind` wire
    /// string. Status surfaces need the number, not the rows.
    async fn count_by_status_kind(&self, status_kind: &str) -> Result<usize>;
    /// Jobs whose status is non-terminal (`pending` / `in_progress` /
    /// `stuck`). Used by admin queries surfacing jobs needing attention.
    async fn list_recoverable(&self) -> Result<Vec<JobRow>>;

    /// Append a state-machine transition audit row.
    async fn record_transition(&self, transition: &JobTransitionRow) -> Result<()>;
    async fn get_transitions(&self, job_id: &JobId) -> Result<Vec<JobTransitionRow>>;
}
