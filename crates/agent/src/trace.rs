//! `SpanRecorder` — facade that owns Step / Span / SpanEvent
//! lifecycle for one session, and `TraceEventStream` — the
//! `tokio::sync::broadcast` bus that downstream observers (cost
//! tracker, hook router, TUI) subscribe to.
//!
//! Replaces the legacy `TraceCollector` + `ObservabilityRecorder`
//! split. See `docs/modules/trace.md` for the design.

use std::sync::Arc;

use aura_model::{JobId, ParallelGroup, SessionId, SpanId, StepId};
use aura_storage::{TraceEventStore, TraceLogEvent, TraceLogEventKind, TraceStore};
use aura_trace::{
    LifecycleOutcome, Span, SpanEvent, SpanEventKind, SpanHandle, SpanKind, Step, StepHandle,
    StepKind, TraceError,
};
use chrono::Utc;
use tokio::sync::broadcast;
use tracing::warn;

type Result<T> = std::result::Result<T, TraceError>;

const TRACE_EVENT_CHANNEL_CAPACITY: usize = 256;

/// One event published on a session's `TraceEventStream`.
///
/// Mirrors the `TraceLogEvent` written to the WAL but carries fully-typed
/// payloads convenient for in-process subscribers.
#[derive(Debug, Clone)]
pub enum TraceEvent {
    StepStarted {
        step_id: StepId,
        job_id: JobId,
        kind: StepKind,
    },
    StepEnded {
        step_id: StepId,
        job_id: JobId,
        outcome: LifecycleOutcome,
    },
    SpanStarted {
        span_id: SpanId,
        step_id: StepId,
        job_id: JobId,
        /// String tag of the kind (`"llm_call"`, `"tool_call"`, etc.).
        kind_tag: &'static str,
    },
    SpanEnded {
        span_id: SpanId,
        step_id: StepId,
        job_id: JobId,
        outcome: LifecycleOutcome,
    },
    SpanEventEmitted(SpanEvent),
    /// Fired specifically by `end_span` for `SpanKind::LlmCall` so
    /// `CostTracker` can subscribe and write cost rows asynchronously.
    /// Carries everything `cost_records` needs.
    LlmSpanEnded {
        span_id: SpanId,
        job_id: JobId,
        session_id: SessionId,
        model_id: String,
        provider: String,
        input_tokens: usize,
        output_tokens: usize,
    },
}

/// Per-session broadcast bus. Cheap to clone; subscribers receive
/// every event published after they call `subscribe()`. Slow
/// subscribers may lag (broadcast::error::RecvError::Lagged) — they
/// should treat that as "I missed N events" and reconcile against the
/// columnar store.
#[derive(Debug, Clone)]
pub struct TraceEventStream {
    sender: broadcast::Sender<TraceEvent>,
}

impl Default for TraceEventStream {
    fn default() -> Self {
        let (sender, _rx) = broadcast::channel(TRACE_EVENT_CHANNEL_CAPACITY);
        Self { sender }
    }
}

impl TraceEventStream {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TraceEvent> {
        self.sender.subscribe()
    }

    /// Number of currently active subscribers. Mostly for tests +
    /// metrics — `publish` is fire-and-forget regardless.
    pub fn receiver_count(&self) -> usize {
        self.sender.receiver_count()
    }

    fn publish(&self, event: TraceEvent) {
        // `send` returns Err only when there are no subscribers; that
        // is not a failure for fire-and-forget audit events.
        let _ = self.sender.send(event);
    }
}

/// Owns the per-session Step / Span / SpanEvent write path.
///
/// One `SpanRecorder` per session — held by `AgentActor` alongside its
/// `JobLifecycle`. Cheap to construct (just three Arc clones + an
/// `mpsc`-style broadcast sender).
pub struct SpanRecorder {
    session_id: SessionId,
    trace_store: Arc<dyn TraceStore>,
    event_store: Arc<dyn TraceEventStore>,
    stream: TraceEventStream,
}

impl SpanRecorder {
    pub fn new(
        session_id: SessionId,
        trace_store: Arc<dyn TraceStore>,
        event_store: Arc<dyn TraceEventStore>,
    ) -> Self {
        Self {
            session_id,
            trace_store,
            event_store,
            stream: TraceEventStream::new(),
        }
    }

    /// Inject an externally-built stream (e.g. when the actor wants
    /// to share one bus with multiple recorders, or wire it into a
    /// gateway-wide aggregator). Defaults to a fresh per-session bus.
    pub fn with_stream(mut self, stream: TraceEventStream) -> Self {
        self.stream = stream;
        self
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn stream(&self) -> &TraceEventStream {
        &self.stream
    }

    // ── Step lifecycle ────────────────────────────────────────────

    /// Open a new `Step` under `job_id`. Persists immediately to the
    /// columnar `steps` table and writes a `StepBegin` event to the
    /// WAL log.
    pub async fn begin_step(&self, job_id: JobId, kind: StepKind) -> Result<StepHandle> {
        let step = Step {
            id: StepId::new(),
            job_id,
            kind: kind.clone(),
            started_at: Utc::now(),
            ended_at: None,
            outcome: LifecycleOutcome::Pending,
        };
        let handle = StepHandle::new(step.id, step.job_id);
        self.trace_store.save_step(&step).await?;
        self.append_log_event(
            job_id,
            TraceLogEventKind::StepBegin {
                step_id: step.id,
                payload: serde_json::json!({ "kind": kind.tag() }),
            },
        )
        .await;
        self.stream.publish(TraceEvent::StepStarted {
            step_id: step.id,
            job_id,
            kind,
        });
        Ok(handle)
    }

    /// Close a step. The recorder loads the step, stamps `ended_at +
    /// outcome`, and re-saves. Emits a WAL `StepEnd` event and a
    /// `StepEnded` stream event.
    pub async fn end_step(&self, handle: StepHandle, outcome: LifecycleOutcome) -> Result<()> {
        let mut step = self
            .trace_store
            .load_step(&handle.step_id)
            .await?
            .ok_or_else(|| TraceError::NotFound(format!("step {}", handle.step_id)))?;
        if step.ended_at.is_some() {
            return Err(TraceError::AlreadyEnded(format!("step {}", handle.step_id)));
        }
        step.ended_at = Some(Utc::now());
        step.outcome = outcome.clone();
        self.trace_store.save_step(&step).await?;
        self.append_log_event(
            handle.job_id,
            TraceLogEventKind::StepEnd {
                step_id: handle.step_id,
                payload: serde_json::json!({
                    "outcome": outcome.tag(),
                }),
            },
        )
        .await;
        self.stream.publish(TraceEvent::StepEnded {
            step_id: handle.step_id,
            job_id: handle.job_id,
            outcome,
        });
        Ok(())
    }

    // ── Span lifecycle ────────────────────────────────────────────

    /// Open a new `Span` under `step`. Persists immediately and
    /// writes a `SpanBegin` event to the WAL log.
    pub async fn begin_span(
        &self,
        step: &StepHandle,
        kind: SpanKind,
        parallel_group: Option<ParallelGroup>,
    ) -> Result<SpanHandle> {
        let span = Span {
            id: SpanId::new(),
            step_id: step.step_id,
            kind: kind.clone(),
            parallel_group,
            started_at: Utc::now(),
            ended_at: None,
            outcome: LifecycleOutcome::Pending,
            events: Vec::new(),
        };
        let handle = SpanHandle::new(span.id, span.step_id);
        let kind_tag = kind.tag();
        self.trace_store.save_span(&span).await?;
        self.append_log_event(
            step.job_id,
            TraceLogEventKind::SpanBegin {
                span_id: span.id,
                step_id: step.step_id,
                payload: serde_json::json!({ "kind": kind_tag }),
            },
        )
        .await;
        self.stream.publish(TraceEvent::SpanStarted {
            span_id: span.id,
            step_id: step.step_id,
            job_id: step.job_id,
            kind_tag,
        });
        Ok(handle)
    }

    /// Close a span with the given final `kind` (containing the
    /// filled-in result fields) + outcome. Replaces the span row
    /// (`INSERT OR REPLACE` semantics from `TraceStore::save_span`).
    /// Emits a `SpanEnd` WAL event, a `SpanEnded` stream event, and
    /// (for `LlmCall` spans) an `LlmSpanEnded` event so cost
    /// subscribers can write `cost_records` asynchronously.
    pub async fn end_span(
        &self,
        handle: SpanHandle,
        job_id: JobId,
        final_kind: SpanKind,
        outcome: LifecycleOutcome,
    ) -> Result<()> {
        let mut span = self
            .trace_store
            .load_span(&handle.span_id)
            .await?
            .ok_or_else(|| TraceError::NotFound(format!("span {}", handle.span_id)))?;
        if span.ended_at.is_some() {
            return Err(TraceError::AlreadyEnded(format!("span {}", handle.span_id)));
        }
        span.ended_at = Some(Utc::now());
        span.outcome = outcome.clone();
        // Cost subscribers care about LlmCall token counts — emit
        // before we move `final_kind` into the row.
        if let SpanKind::LlmCall {
            model_id,
            provider,
            input_tokens,
            output_tokens,
            ..
        } = &final_kind
        {
            self.stream.publish(TraceEvent::LlmSpanEnded {
                span_id: handle.span_id,
                job_id,
                session_id: self.session_id.clone(),
                model_id: model_id.clone(),
                provider: provider.clone(),
                input_tokens: *input_tokens,
                output_tokens: *output_tokens,
            });
        }
        let kind_tag = final_kind.tag();
        span.kind = final_kind;
        self.trace_store.save_span(&span).await?;
        self.append_log_event(
            job_id,
            TraceLogEventKind::SpanEnd {
                span_id: handle.span_id,
                step_id: handle.step_id,
                payload: serde_json::json!({
                    "outcome": outcome.tag(),
                    "kind": kind_tag,
                }),
            },
        )
        .await;
        self.stream.publish(TraceEvent::SpanEnded {
            span_id: handle.span_id,
            step_id: handle.step_id,
            job_id,
            outcome,
        });
        Ok(())
    }

    // ── SpanEvent ────────────────────────────────────────────────

    /// Emit a SpanEvent (sanitize hit / approval / hook degradation).
    /// `seq` is caller-supplied because the recorder does not own
    /// per-span sequence counters — each call site that emits
    /// multiple events on the same span manages its own counter.
    pub async fn emit_event(&self, span_id: SpanId, seq: u32, kind: SpanEventKind) -> Result<()> {
        let event = SpanEvent::new(span_id, seq, kind);
        self.trace_store.append_span_event(&event).await?;
        self.stream.publish(TraceEvent::SpanEventEmitted(event));
        Ok(())
    }

    // ── Recovery ─────────────────────────────────────────────────

    /// Run the half-open span recovery scan. Used at startup, before
    /// the actor accepts messages.
    pub async fn recover_half_open(&self) -> Result<aura_trace::RecoveryReport> {
        self.trace_store.recover_half_open_spans().await
    }

    // ── Internal: WAL log ────────────────────────────────────────

    async fn append_log_event(&self, job_id: JobId, kind: TraceLogEventKind) {
        let event = TraceLogEvent {
            session_id: self.session_id.clone(),
            job_id,
            at: Utc::now(),
            kind,
        };
        if let Err(e) = self.event_store.append(&event).await {
            warn!(error = %e, "failed to append trace WAL event");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_storage::test_support::{MemoryTraceEventStore, MemoryTraceStore};

    fn make_recorder() -> SpanRecorder {
        SpanRecorder::new(
            SessionId::from("cli-test"),
            Arc::new(MemoryTraceStore::new()),
            Arc::new(MemoryTraceEventStore::default()),
        )
    }

    fn dummy_llm_kind() -> SpanKind {
        SpanKind::LlmCall {
            model_id: "claude".into(),
            provider: "anthropic".into(),
            provider_config_hash: "h".into(),
            input_messages: vec![],
            temperature: None,
            output_content: String::new(),
            thinking: None,
            tool_calls: vec![],
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    fn finished_llm_kind(input_tokens: usize, output_tokens: usize) -> SpanKind {
        SpanKind::LlmCall {
            model_id: "claude".into(),
            provider: "anthropic".into(),
            provider_config_hash: "h".into(),
            input_messages: vec![],
            temperature: None,
            output_content: "hi".into(),
            thinking: None,
            tool_calls: vec![],
            input_tokens,
            output_tokens,
        }
    }

    #[tokio::test]
    async fn step_round_trip_emits_events() {
        let rec = make_recorder();
        let mut rx = rec.stream().subscribe();
        let job = JobId::new();
        let h = rec.begin_step(job, StepKind::LlmIteration).await.unwrap();
        rec.end_step(h, LifecycleOutcome::Ok).await.unwrap();
        let started = rx.recv().await.unwrap();
        assert!(matches!(started, TraceEvent::StepStarted { .. }));
        let ended = rx.recv().await.unwrap();
        assert!(matches!(ended, TraceEvent::StepEnded { .. }));
    }

    #[tokio::test]
    async fn span_lifecycle_emits_started_and_ended() {
        let rec = make_recorder();
        let mut rx = rec.stream().subscribe();
        let job = JobId::new();
        let step = rec.begin_step(job, StepKind::LlmIteration).await.unwrap();
        let span = rec.begin_span(&step, dummy_llm_kind(), None).await.unwrap();
        rec.end_span(span, job, finished_llm_kind(10, 5), LifecycleOutcome::Ok)
            .await
            .unwrap();
        // Drain step started + span started + llm span ended + span ended
        let mut tags = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            tags.push(format!("{ev:?}"));
        }
        assert!(tags.iter().any(|t| t.contains("StepStarted")));
        assert!(tags.iter().any(|t| t.contains("SpanStarted")));
        assert!(tags.iter().any(|t| t.contains("LlmSpanEnded")));
        assert!(tags.iter().any(|t| t.contains("SpanEnded")));
    }

    #[tokio::test]
    async fn llm_span_end_emits_token_counts_for_cost_subscriber() {
        let rec = make_recorder();
        let mut rx = rec.stream().subscribe();
        let job = JobId::new();
        let step = rec.begin_step(job, StepKind::LlmIteration).await.unwrap();
        let span = rec.begin_span(&step, dummy_llm_kind(), None).await.unwrap();
        rec.end_span(span, job, finished_llm_kind(123, 45), LifecycleOutcome::Ok)
            .await
            .unwrap();
        // Drain until LlmSpanEnded
        loop {
            match rx.recv().await.unwrap() {
                TraceEvent::LlmSpanEnded {
                    input_tokens,
                    output_tokens,
                    ..
                } => {
                    assert_eq!(input_tokens, 123);
                    assert_eq!(output_tokens, 45);
                    break;
                }
                _ => continue,
            }
        }
    }

    #[tokio::test]
    async fn double_end_step_errors() {
        let rec = make_recorder();
        let job = JobId::new();
        let h = rec.begin_step(job, StepKind::Compression).await.unwrap();
        rec.end_step(h.clone(), LifecycleOutcome::Ok).await.unwrap();
        let err = rec.end_step(h, LifecycleOutcome::Ok).await.unwrap_err();
        assert!(matches!(err, TraceError::AlreadyEnded(_)));
    }

    #[tokio::test]
    async fn emit_event_publishes_and_persists() {
        let rec = make_recorder();
        let mut rx = rec.stream().subscribe();
        let span = SpanId::new();
        rec.emit_event(
            span,
            0,
            SpanEventKind::HookDegraded {
                hook_name: "h".into(),
                timeout_ms: 100,
                phase: aura_model::HookPhase::PreStep,
            },
        )
        .await
        .unwrap();
        let ev = rx.recv().await.unwrap();
        assert!(matches!(ev, TraceEvent::SpanEventEmitted(_)));
    }
}
