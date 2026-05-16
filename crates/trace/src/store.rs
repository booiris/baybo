//! Trace persistence — `TraceStore` reads / writes the columnar main
//! tables (`steps`, `spans`, `span_events`).
//!
//! See `docs/modules/trace.md` for the lifecycle contract.

use async_trait::async_trait;
use aura_model::{JobId, SpanId, StepId};

use crate::{Result, Span, SpanEvent, Step};

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
}
