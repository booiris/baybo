//! Trace persistence — split into `TraceStore` (columnar reads /
//! writes for `Step` / `Span` / `SpanEvent`) and `TraceEventStore`
//! (the append-only WAL log used for crash recovery).
//!
//! See `docs/modules/trace.md` for the two-layer WAL contract.

use async_trait::async_trait;
use aura_model::{JobId, SessionId, SpanId, StepId};
use aura_trace::{RecoveryReport, Span, SpanEvent, Step, TraceError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type Result<T> = std::result::Result<T, TraceError>;

/// Reads / writes the columnar main tables (`steps`, `spans`,
/// `span_events`). Writes happen at Step/Span end (per `D3` fence
/// protocol — see `docs/modules/trace.md`); the WAL log catches
/// in-flight begin/end events.
#[async_trait]
pub trait TraceStore: Send + Sync {
    // -- Step --
    async fn save_step(&self, step: &Step) -> Result<()>;
    async fn load_step(&self, step_id: &StepId) -> Result<Option<Step>>;
    async fn list_steps_by_job(&self, job_id: &JobId) -> Result<Vec<Step>>;

    // -- Span --
    async fn save_span(&self, span: &Span) -> Result<()>;
    async fn load_span(&self, span_id: &SpanId) -> Result<Option<Span>>;
    async fn list_spans_by_step(&self, step_id: &StepId) -> Result<Vec<Span>>;

    // -- SpanEvent --
    async fn append_span_event(&self, event: &SpanEvent) -> Result<()>;
    async fn list_span_events(&self, span_id: &SpanId) -> Result<Vec<SpanEvent>>;

    // -- Recovery --
    /// Find every span with `started_at IS NOT NULL AND ended_at IS NULL
    /// AND deleted_at IS NULL`, mark each `Cancelled { SystemCrash }`,
    /// stamp `ended_at = now`, and return the per-job grouping so
    /// `JobLifecycle::recover_interrupted` can fold the IDs into each
    /// parent job's `partial_artifacts`.
    ///
    /// Idempotent: re-running over an already-recovered set returns an
    /// empty report.
    async fn recover_half_open_spans(&self) -> Result<RecoveryReport>;
}

/// Append-only WAL log of begin / end events.
///
/// Every Step/Span begin and end writes one small row here with
/// fsync-immediate semantics. Used by the recovery scan as the source
/// of truth when the columnar tables are behind. Compactable at the
/// retention horizon.
#[async_trait]
pub trait TraceEventStore: Send + Sync {
    /// Append a single event. Must fsync before returning.
    async fn append(&self, event: &TraceLogEvent) -> Result<()>;
    /// Replay events for one session in chronological order. Used by
    /// recovery and by `replay(session_id, until_step_id)` queries.
    async fn replay_session(&self, session_id: &SessionId) -> Result<Vec<TraceLogEvent>>;
    /// Compact events older than `cutoff`. Soft-delete on the row;
    /// the actual purge is governed by retention policy.
    async fn compact_before(&self, cutoff: DateTime<Utc>) -> Result<u64>;
}

/// One WAL event. Compact and fixed-shape; payload-typed by `kind`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceLogEvent {
    pub session_id: SessionId,
    pub job_id: JobId,
    pub at: DateTime<Utc>,
    pub kind: TraceLogEventKind,
}

/// Closed enum of WAL event types. Payload `Value` carries the
/// minimum context needed to reconstruct the corresponding row in the
/// columnar tables — typically just the affected entity ID and the
/// transition's data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TraceLogEventKind {
    StepBegin {
        step_id: StepId,
        payload: Value,
    },
    StepEnd {
        step_id: StepId,
        payload: Value,
    },
    SpanBegin {
        span_id: SpanId,
        step_id: StepId,
        payload: Value,
    },
    SpanEnd {
        span_id: SpanId,
        step_id: StepId,
        payload: Value,
    },
    JobTransition {
        from: String,
        to: String,
    },
}
