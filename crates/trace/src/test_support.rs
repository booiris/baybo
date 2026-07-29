//! In-memory `TraceStore` for downstream tests.
//!
//! Gated behind the `test-support` cargo feature so it never ships in
//! release builds. Lives in `baybo-trace` (next to the row conversions)
//! so crates that depend on `baybo-trace` can spin up a fake store
//! without pulling the sqlite adapter.

use std::collections::HashMap;

use async_trait::async_trait;
use baybo_model::{SpanId, StepId, TurnId};
use baybo_store::trace::Result;
use baybo_store::{SpanEventRow, SpanRow, StepRow, TraceStore};
use parking_lot::Mutex;

use crate::{LifecycleState, Span, Step};

/// In-memory `TraceStore` for tests. Stores rows verbatim; the
/// `list_*_by_*` filters deserialize to read the parent id out of the
/// `data` blob (the sqlite backend uses generated columns instead).
#[derive(Debug, Default)]
pub struct MemoryTraceStore {
    steps: Mutex<HashMap<StepId, StepRow>>,
    spans: Mutex<HashMap<SpanId, SpanRow>>,
    span_events: Mutex<HashMap<SpanId, Vec<SpanEventRow>>>,
}

impl MemoryTraceStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl TraceStore for MemoryTraceStore {
    async fn save_step(&self, step: &StepRow) -> Result<()> {
        self.steps.lock().insert(step.id, step.clone());
        Ok(())
    }

    async fn load_step(&self, step_id: &StepId) -> Result<Option<StepRow>> {
        Ok(self.steps.lock().get(step_id).cloned())
    }

    async fn list_steps_by_turn(&self, turn_id: &TurnId) -> Result<Vec<StepRow>> {
        Ok(self
            .steps
            .lock()
            .values()
            .filter(|r| Step::from_row((*r).clone()).is_ok_and(|s| &s.turn_id == turn_id))
            .cloned()
            .collect())
    }

    async fn list_unfinished_steps(&self) -> Result<Vec<StepRow>> {
        let unfinished_span_steps: std::collections::HashSet<StepId> = self
            .spans
            .lock()
            .values()
            .filter_map(|r| Span::from_row(r.clone()).ok())
            .filter(|s| matches!(s.outcome, LifecycleState::Pending) || s.ended_at.is_none())
            .map(|s| s.step_id)
            .collect();
        Ok(self
            .steps
            .lock()
            .values()
            .filter(|r| {
                Step::from_row((*r).clone()).is_ok_and(|s| {
                    matches!(s.outcome, LifecycleState::Pending)
                        || s.ended_at.is_none()
                        || unfinished_span_steps.contains(&s.id)
                })
            })
            .cloned()
            .collect())
    }

    async fn save_span(&self, span: &SpanRow) -> Result<()> {
        self.spans.lock().insert(span.id, span.clone());
        Ok(())
    }

    async fn load_span(&self, span_id: &SpanId) -> Result<Option<SpanRow>> {
        Ok(self.spans.lock().get(span_id).cloned())
    }

    async fn list_spans_by_step(&self, step_id: &StepId) -> Result<Vec<SpanRow>> {
        Ok(self
            .spans
            .lock()
            .values()
            .filter(|r| Span::from_row((*r).clone()).is_ok_and(|s| &s.step_id == step_id))
            .cloned()
            .collect())
    }

    async fn list_spans_by_turn(&self, turn_id: &TurnId) -> Result<Vec<SpanRow>> {
        let step_ids: std::collections::HashSet<StepId> = self
            .steps
            .lock()
            .values()
            .filter_map(|r| Step::from_row(r.clone()).ok())
            .filter(|s| &s.turn_id == turn_id)
            .map(|s| s.id)
            .collect();
        let mut spans: Vec<(chrono::DateTime<chrono::Utc>, SpanRow)> = self
            .spans
            .lock()
            .values()
            .filter_map(|r| {
                Span::from_row(r.clone())
                    .ok()
                    .filter(|s| step_ids.contains(&s.step_id))
                    .map(|s| (s.started_at, r.clone()))
            })
            .collect();
        spans.sort_by_key(|(started_at, _)| *started_at);
        Ok(spans.into_iter().map(|(_, r)| r).collect())
    }

    async fn trace_counts_by_turn(&self, turn_id: &TurnId) -> Result<(usize, usize)> {
        let step_ids: std::collections::HashSet<StepId> = self
            .steps
            .lock()
            .values()
            .filter_map(|r| Step::from_row(r.clone()).ok())
            .filter(|s| &s.turn_id == turn_id)
            .map(|s| s.id)
            .collect();
        let spans = self
            .spans
            .lock()
            .values()
            .filter(|r| Span::from_row((*r).clone()).is_ok_and(|s| step_ids.contains(&s.step_id)))
            .count();
        Ok((step_ids.len(), spans))
    }

    async fn append_span_event(&self, event: &SpanEventRow) -> Result<()> {
        self.span_events
            .lock()
            .entry(event.span_id)
            .or_default()
            .push(event.clone());
        Ok(())
    }

    async fn list_span_events(&self, span_id: &SpanId) -> Result<Vec<SpanEventRow>> {
        Ok(self
            .span_events
            .lock()
            .get(span_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn list_span_events_for_spans(&self, span_ids: &[SpanId]) -> Result<Vec<SpanEventRow>> {
        let events = self.span_events.lock();
        let mut out = Vec::new();
        for id in span_ids {
            if let Some(evs) = events.get(id) {
                out.extend(evs.iter().cloned());
            }
        }
        Ok(out)
    }
}
