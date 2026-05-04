//! Trace persistence — `TraceStore` reads / writes the columnar main
//! tables (`steps`, `spans`, `span_events`).
//!
//! See `docs/modules/trace.md` for the lifecycle contract.

use async_trait::async_trait;
use aura_model::{JobId, SpanId, StepId};
use aura_trace::{RecoveryReport, Span, SpanEvent, Step, TraceError};

pub type Result<T> = std::result::Result<T, TraceError>;

/// Reads / writes the columnar main tables (`steps`, `spans`,
/// `span_events`).
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
