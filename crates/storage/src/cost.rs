use async_trait::async_trait;
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
/// cost without knowing which call caused it. `originating_session_deleted_at`
/// mirrors `sessions.deleted_at` of the originating session — populated
/// by `SessionStore::soft_delete` so cost UIs can render
/// "source session deleted" without joining back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostRecord {
    pub user_id: String,
    pub session_id: aura_model::SessionId,
    pub job_id: aura_model::JobId,
    pub span_id: aura_model::SpanId,
    pub model: String,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cost_usd: f64,
    pub timestamp: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub originating_session_deleted_at: Option<DateTime<Utc>>,
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

/// Cached per-user-per-month total. Populated lazily by
/// `CostSubscriber` after each `cost_records` write; read by
/// `CostGuard` for monthly-quota checks. Carries `deleted_at` so the
/// retention sweep can purge stale entries (raw `cost_records` is
/// the audit-truth and never deleted).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMonthlyCost {
    pub user_id: String,
    /// Calendar month tag — `YYYY-MM` (UTC).
    pub month: String,
    pub cost_usd: f64,
    pub updated_at: DateTime<Utc>,
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

    /// Return the sum of `cost_usd` for a user within the given time range.
    async fn sum_user(&self, user_id: &str, range: TimeRange) -> CostResult<f64>;

    // ── Cached user-monthly aggregate (lazy materialisation, with
    //    soft-delete + retention) ────────────────────────────────

    /// Add `delta_usd` to the (`user_id`, `month`) cache row,
    /// inserting if absent. Resets `deleted_at` so re-incrementing a
    /// soft-deleted row revives it.
    async fn bump_user_monthly_cost(
        &self,
        user_id: &str,
        month: &str,
        delta_usd: f64,
    ) -> CostResult<()>;

    /// Read the cached monthly total. Returns `None` when the row is
    /// missing or soft-deleted (caller should recompute from raw
    /// `cost_records` and re-bump).
    async fn get_user_monthly_cost(
        &self,
        user_id: &str,
        month: &str,
    ) -> CostResult<Option<UserMonthlyCost>>;

    /// Soft-delete every cached row whose `updated_at` is strictly
    /// before `cutoff`. Returns the number of rows touched. Periodic
    /// invocation lives in `aura-janitor` (see retention policy).
    async fn purge_user_monthly_cost_older_than(&self, cutoff: DateTime<Utc>) -> CostResult<u64>;
}
