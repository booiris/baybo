use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{JobId, MicroUsd, SessionId, SpanId};

/// The smallest auditable billing unit.
///
/// Every record is associated with both a `job_id` and a `span_id` (the
/// LLM span that drove the spend), so the system never knows a cost
/// without knowing which call caused it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostRecord {
    pub user_id: String,
    pub session_id: SessionId,
    pub job_id: JobId,
    pub span_id: SpanId,
    pub model: String,
    pub input_tokens: usize,
    pub output_tokens: usize,
    /// Anthropic prompt-cache: input tokens served from the cache.
    /// Counted separately from `input_tokens` so cache-discounted pricing
    /// can be applied later. 0 for providers that don't report cache usage.
    pub cached_input_tokens: usize,
    /// Anthropic prompt-cache: input tokens written into the cache.
    pub cache_creation_input_tokens: usize,
    /// Spend for this single LLM call. Stored as integer micro-USD — see
    /// [`MicroUsd`] for the rationale (no float drift across aggregations
    /// or quota checks).
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
