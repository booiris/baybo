use async_trait::async_trait;
use aura_model::MicroUsd;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CostError {
    #[error("cost storage error: {0}")]
    Storage(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type CostResult<T> = std::result::Result<T, CostError>;

/// The smallest auditable billing unit.
///
/// Every record is associated with both a `job_id` and a `span_id`
/// (the LLM span that drove the spend), so the system never knows a
/// cost without knowing which call caused it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostRecord {
    pub user_id: String,
    pub session_id: aura_model::SessionId,
    pub job_id: aura_model::JobId,
    pub span_id: aura_model::SpanId,
    pub model: String,
    pub input_tokens: usize,
    pub output_tokens: usize,
    /// Anthropic prompt-cache: input tokens served from the cache.
    /// Counted separately from `input_tokens` so cache-discounted
    /// pricing can be applied later. 0 for providers that don't
    /// report cache usage.
    pub cached_input_tokens: usize,
    /// Anthropic prompt-cache: input tokens written into the cache.
    pub cache_creation_input_tokens: usize,
    /// Spend for this single LLM call. Stored as integer micro-USD —
    /// see [`MicroUsd`] for the rationale (no float drift across
    /// aggregations or quota checks).
    pub cost_usd: MicroUsd,
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
    pub total_cost_usd: MicroUsd,
    pub total_input_tokens: usize,
    pub total_output_tokens: usize,
    pub total_cached_input_tokens: usize,
    pub total_cache_creation_input_tokens: usize,
    pub record_count: usize,
}

/// Persistence backend for cost records.
#[async_trait]
pub trait CostStore: Send + Sync {
    /// Persist a single cost record.
    async fn record(&self, record: &CostRecord) -> CostResult<()>;

    /// Return all records for a user within the given time range.
    async fn query_user(&self, user_id: &str, range: TimeRange) -> CostResult<Vec<CostRecord>>;

    /// Return an aggregated summary of all records within the given time range.
    async fn query_global(&self, range: TimeRange) -> CostResult<CostSummary>;

    /// Return the raw `CostRecord`s within the time range (any user).
    /// Powers analytics aggregations that need a per-day or per-model
    /// breakdown the summary methods don't expose.
    async fn query_records_in_range(&self, range: TimeRange) -> CostResult<Vec<CostRecord>>;

    /// Return the aggregated cost summary for a single session. Drives
    /// `QueryApi::cost_summary(CostScope::Session)`.
    async fn query_session(&self, session_id: &aura_model::SessionId) -> CostResult<CostSummary>;

    /// Return the aggregated cost summary for a single job. Drives
    /// `QueryApi::cost_summary(CostScope::Job)`.
    async fn query_job(&self, job_id: &aura_model::JobId) -> CostResult<CostSummary>;
}
