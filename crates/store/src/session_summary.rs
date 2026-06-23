//! Persistence interface for the per-session summary metadata row.
//!
//! On-disk content lives at `<workspace>/state/sessions/<session_id>/summary.md`;
//! the row in `session_summaries` is the durable, queryable index that pairs
//! each summary with the message-ordinal cursor it covers. See
//! [`docs/background-compression.md`](../../../docs/background-compression.md)
//! for the full design.

use async_trait::async_trait;
use baybo_model::SessionId;
use chrono::{DateTime, Utc};

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// One row of `session_summaries`. `cursor` is the
/// `session_messages.ordinal` of the most-recent message included in
/// the last successful summary pass. `pass_count` increments
/// monotonically; `error_count` is **telemetry only** — it does not
/// gate triggers (a persistent failure burns one LLM call per trigger
/// event until conditions self-resolve).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummaryRow {
    pub session_id: SessionId,
    pub cursor: i64,
    pub pass_count: i64,
    pub updated_at: DateTime<Utc>,
    /// Cumulative micro-USD spent on this session's summary passes.
    /// INTEGER, never REAL — same `feedback_money_no_float` invariant
    /// as `cost_records.cost_usd`.
    pub cost_micros: i64,
    pub model_id: String,
    pub span_id: String,
    pub error_count: i64,
}

/// Per-session summary metadata persistence.
///
/// All mutations are idempotent at the row level: `upsert_success`
/// and `bump_error_count` either land or fail without partial state.
/// `delete` is fired on parent-session deletion via the `ON DELETE
/// CASCADE` foreign key, so explicit calls are only needed for
/// orphan-reaping flows.
#[async_trait]
pub trait SessionSummaryStore: Send + Sync {
    /// Read the row for `session_id`. Returns `Ok(None)` when no
    /// summary has ever been successfully written for this session.
    async fn get(&self, session_id: &SessionId) -> Result<Option<SessionSummaryRow>>;

    /// Upsert after a successful summary pass. Increments `pass_count`
    /// from the prior value (or 0 on first insert) and adds
    /// `cost_micros_delta` to the cumulative `cost_micros`. Resets
    /// `error_count` to 0 — a successful pass clears prior errors so
    /// `error_count` represents "consecutive failures since the last
    /// success" if interpreted that way (current design treats it as
    /// telemetry only, but the reset behaviour is conservative).
    async fn upsert_success(
        &self,
        session_id: &SessionId,
        cursor: i64,
        cost_micros_delta: i64,
        model_id: &str,
        span_id: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<()>;

    /// Increment `error_count` for `session_id`. Inserts a new row
    /// with `cursor=0, pass_count=0` if none exists — even sessions
    /// that have never produced a successful summary should be able
    /// to surface failure telemetry. `model_id` / `span_id` track the
    /// last-attempted call so operators can find the failing trace.
    async fn bump_error_count(
        &self,
        session_id: &SessionId,
        model_id: &str,
        span_id: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<()>;

    /// Hard-delete the row. Idempotent; returns `Ok(false)` if the
    /// row did not exist. Cascade from `sessions` is the normal path;
    /// this exists for orphan-reap of FS files whose session_id has
    /// no DB row.
    async fn delete(&self, session_id: &SessionId) -> Result<bool>;

    /// List every parent_session_id that has a metadata row. Used by
    /// the FS orphan reaper at startup: it scans
    /// `<workspace>/state/sessions/*/summary.md` and deletes files
    /// whose session_id is **not** in this list. Only the IDs are
    /// returned — `Vec<SessionId>` is small even for installations
    /// with many sessions.
    async fn list_session_ids(&self) -> Result<Vec<SessionId>>;
}
