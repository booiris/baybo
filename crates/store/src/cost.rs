use async_trait::async_trait;
use baybo_model::{CostRecord, CostSummary, JobId, SessionId, TimeRange};

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Group key for [`CostStore::query_range_grouped`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostGroupKey {
    /// UTC day, rendered `YYYY-MM-DD`.
    Day,
    /// Model id string.
    Model,
    /// `CallReason` wire token (`chat`, `tool:<name>`, …). Rows written
    /// before the reason column existed group under the empty string.
    Reason,
}

/// One bucket of a grouped cost aggregate: the group key rendered as a
/// string plus the summed totals.
#[derive(Debug, Clone)]
pub struct CostGroupBucket {
    pub key: String,
    pub summary: CostSummary,
}

/// Persistence backend for cost records.
#[async_trait]
pub trait CostStore: Send + Sync {
    /// Persist a single cost record.
    async fn record(&self, record: &CostRecord) -> Result<()>;

    /// Return all records for a user within the given time range.
    async fn query_user(&self, user_id: &str, range: TimeRange) -> Result<Vec<CostRecord>>;

    /// Aggregated summary of one user's records within the range — the
    /// SUM-in-SQL sibling of [`Self::query_user`] for callers that only
    /// need the totals.
    async fn query_user_summary(&self, user_id: &str, range: TimeRange) -> Result<CostSummary>;

    /// Return an aggregated summary of all records within the given time range.
    async fn query_global(&self, range: TimeRange) -> Result<CostSummary>;

    /// Return the raw `CostRecord`s within the time range (any user).
    async fn query_records_in_range(&self, range: TimeRange) -> Result<Vec<CostRecord>>;

    /// Aggregates over the range grouped by `key`, one bucket per
    /// distinct key value (days with no records yield no bucket).
    /// Powers analytics breakdowns without materialising every record.
    async fn query_range_grouped(
        &self,
        range: TimeRange,
        key: CostGroupKey,
    ) -> Result<Vec<CostGroupBucket>>;

    /// Return the aggregated cost summary for a single session.
    async fn query_session(&self, session_id: &SessionId) -> Result<CostSummary>;

    /// Per-job aggregated summaries for one session, one bucket per
    /// `job_id` (rendered as the bucket key). One grouped query
    /// replaces a `query_job` fan-out over the session's jobs.
    async fn query_session_by_job(&self, session_id: &SessionId) -> Result<Vec<CostGroupBucket>>;

    /// Return the aggregated cost summary for a single job.
    async fn query_job(&self, job_id: &JobId) -> Result<CostSummary>;
}
