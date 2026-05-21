//! Persistence interface for the per-session summary metadata row.
//!
//! On-disk content lives at `<workspace>/state/sessions/<session_id>/summary.md`;
//! the row in `session_summaries` is the durable, queryable index that pairs
//! each summary with the message-ordinal cursor it covers. See
//! [`docs/background-compression.md`](../../../docs/background-compression.md)
//! for the full design.

use async_trait::async_trait;
use aura_model::SessionId;
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
    /// `true` while a `BackgroundCompressionRunner` pass is active for
    /// this parent. Set by the trigger gate before emitting a
    /// `SystemSpawnRequest` and cleared by
    /// `record_summary_success`/`record_summary_failure`. The gate
    /// reads this flag to enforce the at-most-one-in-flight invariant
    /// without consulting the maintenance session row (which is kept
    /// as audit history).
    pub in_flight: bool,
    /// Opaque owner token stamped onto the `in_flight = true` mark.
    /// Used by the runner's defensive post-pass cleanup to do a
    /// compare-and-clear (`WHERE in_flight_owner = ?`) so a stale
    /// cleanup from a finished pass cannot wipe a newer pass'
    /// in-flight mark. `None` whenever `in_flight` is false.
    pub in_flight_owner: Option<String>,
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

    /// Set the `in_flight` flag for `session_id`. UPSERTs a placeholder
    /// row (cursor=0, pass_count=0, model_id="", span_id="") if none
    /// exists, so the gate can mark a session in-flight before its
    /// first successful pass has ever recorded. Other columns on an
    /// existing row are left untouched.
    ///
    /// `owner` is the opaque token to stamp alongside the flag — pass
    /// `Some(token)` when setting `in_flight = true`, `None` when
    /// clearing. The clear path overwrites the prior owner token to
    /// NULL unconditionally; for owner-checked clears that don't want
    /// to clobber a newer pass' mark, use [`Self::clear_in_flight_if_owned`]
    /// instead.
    async fn set_in_flight(
        &self,
        session_id: &SessionId,
        in_flight: bool,
        owner: Option<&str>,
        updated_at: DateTime<Utc>,
    ) -> Result<()>;

    /// Compare-and-clear: set `in_flight = 0` only when the row's
    /// `in_flight_owner` matches `owner`. Returns `Ok(true)` when the
    /// row was updated (the caller's pass still owned the mark) and
    /// `Ok(false)` when no row matched (a newer pass took ownership,
    /// or the row was already cleared by `record_summary_*`). Used by
    /// the runner's defensive post-pass cleanup so a stale Pass A
    /// finishing after a Pass B already remarked the parent cannot
    /// wipe Pass B's mark.
    async fn clear_in_flight_if_owned(
        &self,
        session_id: &SessionId,
        owner: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<bool>;

    /// Reset `in_flight` to 0 across the entire table. Called once at
    /// startup by the orphan reaper: a process that just started has
    /// no in-flight passes by definition, so any leftover `in_flight = 1`
    /// from a crash mid-mark (between `set_in_flight(true)` and the
    /// router's `create_maintenance_session`) is stale. Also clears
    /// `in_flight_owner` (no live process owns these tokens any more).
    async fn clear_all_in_flight(&self) -> Result<()>;

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
