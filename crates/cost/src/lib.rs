pub mod error;
pub mod tracker;

pub use error::CostError;
pub use tracker::CostTracker;

pub type Result<T> = std::result::Result<T, CostError>;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The smallest auditable billing unit.
/// Every record is associated with both a `job_id` and a `trace_span_id`,
/// so the system never knows a cost without knowing which call caused it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostRecord {
    pub user_id: String,
    pub session_id: String,
    pub job_id: String,
    pub trace_span_id: String,
    pub model: String,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cost_usd: f64,
    pub timestamp: DateTime<Utc>,
}

/// A half-open time range `[from, to)` used for cost queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeRange {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

/// Aggregated cost information over a time range.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CostSummary {
    pub total_cost_usd: f64,
    pub total_input_tokens: usize,
    pub total_output_tokens: usize,
    pub record_count: usize,
}

/// Persistence backend for cost records.
///
/// Implementations live in the `storage` crate (in-memory for tests, SQLite for production).
#[async_trait]
pub trait CostStore: Send + Sync {
    /// Persist a single cost record.
    async fn record(&self, record: &CostRecord) -> crate::Result<()>;

    /// Return all records for a user within the given time range.
    async fn query_user(&self, user_id: &str, range: TimeRange) -> crate::Result<Vec<CostRecord>>;

    /// Return an aggregated summary of all records within the given time range.
    async fn query_global(&self, range: TimeRange) -> crate::Result<CostSummary>;

    /// Return the sum of `cost_usd` for a user within the given time range.
    async fn sum_user(&self, user_id: &str, range: TimeRange) -> crate::Result<f64>;
}
