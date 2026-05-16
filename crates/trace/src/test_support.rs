//! In-memory `TraceStore` for downstream tests.
//!
//! Gated behind the `test-support` cargo feature so it never ships in
//! release builds. Lives in `aura-trace` (next to the trait it
//! implements) so crates that depend on `aura-trace` but not on
//! `aura-storage` can still spin up a fake store for unit tests.

use std::collections::HashMap;

use async_trait::async_trait;
use aura_model::{JobId, SpanId, StepId};
use parking_lot::Mutex;

use crate::store::TraceStore;
use crate::{Result, Span, SpanEvent, Step};

/// In-memory `TraceStore` for tests. Steps are keyed by `StepId`,
/// spans by `SpanId`, span events by `(SpanId, seq)`.
#[derive(Debug, Default)]
pub struct MemoryTraceStore {
    steps: Mutex<HashMap<StepId, Step>>,
    spans: Mutex<HashMap<SpanId, Span>>,
    span_events: Mutex<HashMap<SpanId, Vec<SpanEvent>>>,
}

impl MemoryTraceStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.spans.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl TraceStore for MemoryTraceStore {
    async fn save_step(&self, step: &Step) -> Result<()> {
        self.steps.lock().insert(step.id, step.clone());
        Ok(())
    }

    async fn load_step(&self, step_id: &StepId) -> Result<Option<Step>> {
        Ok(self.steps.lock().get(step_id).cloned())
    }

    async fn list_steps_by_job(&self, job_id: &JobId) -> Result<Vec<Step>> {
        let mut out: Vec<Step> = self
            .steps
            .lock()
            .values()
            .filter(|s| &s.job_id == job_id)
            .cloned()
            .collect();
        out.sort_by_key(|s| s.started_at);
        Ok(out)
    }

    async fn save_span(&self, span: &Span) -> Result<()> {
        self.spans.lock().insert(span.id, span.clone());
        Ok(())
    }

    async fn load_span(&self, span_id: &SpanId) -> Result<Option<Span>> {
        Ok(self.spans.lock().get(span_id).cloned())
    }

    async fn list_spans_by_step(&self, step_id: &StepId) -> Result<Vec<Span>> {
        let mut out: Vec<Span> = self
            .spans
            .lock()
            .values()
            .filter(|s| &s.step_id == step_id)
            .cloned()
            .collect();
        out.sort_by_key(|s| s.started_at);
        Ok(out)
    }

    async fn append_span_event(&self, event: &SpanEvent) -> Result<()> {
        self.span_events
            .lock()
            .entry(event.span_id)
            .or_default()
            .push(event.clone());
        Ok(())
    }

    async fn list_span_events(&self, span_id: &SpanId) -> Result<Vec<SpanEvent>> {
        Ok(self
            .span_events
            .lock()
            .get(span_id)
            .cloned()
            .unwrap_or_default())
    }
}
