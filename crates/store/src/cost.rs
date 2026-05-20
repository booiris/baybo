use async_trait::async_trait;
use aura_model::{CostRecord, CostSummary, JobId, SessionId, TimeRange};

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Persistence backend for cost records.
#[async_trait]
pub trait CostStore: Send + Sync {
    /// Persist a single cost record.
    async fn record(&self, record: &CostRecord) -> Result<()>;

    /// Return all records for a user within the given time range.
    async fn query_user(&self, user_id: &str, range: TimeRange) -> Result<Vec<CostRecord>>;

    /// Return an aggregated summary of all records within the given time range.
    async fn query_global(&self, range: TimeRange) -> Result<CostSummary>;

    /// Return the raw `CostRecord`s within the time range (any user). Powers
    /// analytics aggregations that need a per-day or per-model breakdown the
    /// summary methods don't expose.
    async fn query_records_in_range(&self, range: TimeRange) -> Result<Vec<CostRecord>>;

    /// Return the aggregated cost summary for a single session.
    async fn query_session(&self, session_id: &SessionId) -> Result<CostSummary>;

    /// Return the aggregated cost summary for a single job.
    async fn query_job(&self, job_id: &JobId) -> Result<CostSummary>;
}
