use async_trait::async_trait;
use baybo_model::{JobId, SpanId, StepId};

use crate::StorageError;

pub type Result<T> = std::result::Result<T, StorageError>;

/// Persistence row for a trace `Step`. The full `Step` is serialized into
/// `data`; the backend derives its queryable columns (`job_id`,
/// `started_at`) from that JSON. `baybo-trace` owns the `Step` <-> row
/// conversion (`Step::to_row` / `Step::from_row`).
#[derive(Debug, Clone)]
pub struct StepRow {
    pub id: StepId,
    pub data: String,
}

/// Persistence row for a trace `Span` (serialized into `data`).
#[derive(Debug, Clone)]
pub struct SpanRow {
    pub id: SpanId,
    pub data: String,
}

/// Persistence row for a `SpanEvent`. `span_id` + `seq` are the natural
/// key; the event is serialized into `data`.
#[derive(Debug, Clone)]
pub struct SpanEventRow {
    pub span_id: SpanId,
    pub seq: u32,
    pub data: String,
}

/// Persistence backend for the trace tables (`steps`, `spans`,
/// `span_events`). Trades in rows; `baybo-trace` converts to/from its rich
/// `Step` / `Span` / `SpanEvent` types at the boundary so the lifecycle
/// recorder's logic stays in `baybo-trace`.
#[async_trait]
pub trait TraceStore: Send + Sync {
    async fn save_step(&self, step: &StepRow) -> Result<()>;
    async fn load_step(&self, step_id: &StepId) -> Result<Option<StepRow>>;
    async fn list_steps_by_job(&self, job_id: &JobId) -> Result<Vec<StepRow>>;
    /// List steps that still need trace recovery: either the step row itself is
    /// open, or at least one child span is open. Recovery uses this to find
    /// detached trace work under jobs that are already terminal.
    async fn list_unfinished_steps(&self) -> Result<Vec<StepRow>>;

    async fn save_span(&self, span: &SpanRow) -> Result<()>;
    async fn load_span(&self, span_id: &SpanId) -> Result<Option<SpanRow>>;
    async fn list_spans_by_step(&self, step_id: &StepId) -> Result<Vec<SpanRow>>;

    async fn append_span_event(&self, event: &SpanEventRow) -> Result<()>;
    async fn list_span_events(&self, span_id: &SpanId) -> Result<Vec<SpanEventRow>>;
}
