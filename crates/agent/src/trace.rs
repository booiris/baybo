//! `SpanRecorder` — facade that owns Step / Span / SpanEvent
//! lifecycle for one session, and `TraceEventStream` — the
//! `tokio::sync::broadcast` bus that downstream observers (cost
//! tracker, hook router, TUI) subscribe to.
//!
//! Replaces the legacy `TraceCollector` + `ObservabilityRecorder`
//! split. See `docs/modules/trace.md` for the design.

use std::sync::Arc;

use aura_model::{JobId, ParallelGroup, SessionId, SpanId, StepId};
use aura_storage::TraceStore;
use aura_trace::{
    LifecycleOutcome, Span, SpanEvent, SpanEventKind, SpanFinalize, SpanHandle, SpanKind, Step,
    StepHandle, StepKind, TraceError,
};
use chrono::Utc;
use tokio::sync::broadcast;

type Result<T> = std::result::Result<T, TraceError>;

const TRACE_EVENT_CHANNEL_CAPACITY: usize = 256;

/// One event published on a session's `TraceEventStream`.
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
    /// Carries everything `cost_records` needs, including the owning
    /// user (so `user_monthly_cost` rolls up per-user-per-month rather
    /// than collapsing every event into one (`""`, month) row).
    LlmSpanEnded {
        span_id: SpanId,
        job_id: JobId,
        session_id: SessionId,
        user_id: String,
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

    pub(crate) fn publish(&self, event: TraceEvent) {
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
    user_id: String,
    trace_store: Arc<dyn TraceStore>,
    stream: TraceEventStream,
}

impl SpanRecorder {
    pub fn new(session_id: SessionId, user_id: String, trace_store: Arc<dyn TraceStore>) -> Self {
        Self {
            session_id,
            user_id,
            trace_store,
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
    /// columnar `steps` table.
    pub async fn begin_step(&self, job_id: JobId, kind: StepKind) -> Result<StepHandle> {
        let started_at = Utc::now();
        let step_id = StepId::new();
        let step = Step {
            id: step_id,
            job_id,
            kind: kind.clone(),
            started_at,
            ended_at: None,
            outcome: LifecycleOutcome::Pending,
        };
        self.trace_store.save_step(&step).await?;
        self.stream.publish(TraceEvent::StepStarted {
            step_id,
            job_id,
            kind: kind.clone(),
        });
        Ok(StepHandle::new(step_id, job_id, kind, started_at))
    }

    /// Close a step. Constructs the closed `Step` row from the handle
    /// (which carries the begin-time `kind` + `started_at`) so the
    /// columnar write is a single INSERT OR REPLACE — no SELECT.
    pub async fn end_step(&self, handle: StepHandle, outcome: LifecycleOutcome) -> Result<()> {
        let step = Step {
            id: handle.step_id,
            job_id: handle.job_id,
            kind: handle.kind,
            started_at: handle.started_at,
            ended_at: Some(Utc::now()),
            outcome: outcome.clone(),
        };
        self.trace_store.save_step(&step).await?;
        self.stream.publish(TraceEvent::StepEnded {
            step_id: handle.step_id,
            job_id: handle.job_id,
            outcome,
        });
        Ok(())
    }

    // ── Span lifecycle ────────────────────────────────────────────

    /// Open a new `Span` under `step`. Persists immediately.
    pub async fn begin_span(
        &self,
        step: &StepHandle,
        kind: SpanKind,
        parallel_group: Option<ParallelGroup>,
    ) -> Result<SpanHandle> {
        let started_at = Utc::now();
        let span_id = SpanId::new();
        let span = Span {
            id: span_id,
            step_id: step.step_id,
            kind: kind.clone(),
            parallel_group,
            started_at,
            ended_at: None,
            outcome: LifecycleOutcome::Pending,
            events: Vec::new(),
        };
        let kind_tag = kind.tag();
        self.trace_store.save_span(&span).await?;
        self.stream.publish(TraceEvent::SpanStarted {
            span_id,
            step_id: step.step_id,
            job_id: step.job_id,
            kind_tag,
        });
        Ok(SpanHandle::new(
            span_id,
            step.step_id,
            kind,
            started_at,
            parallel_group,
        ))
    }

    /// Close a span. Merges the [`SpanFinalize`] payload into the
    /// begin-time kind on the handle, writes via INSERT OR REPLACE
    /// (no SELECT), and publishes the stream events. For `LlmCall`
    /// spans, also publishes an `LlmSpanEnded` event so cost
    /// subscribers can write `cost_records` asynchronously.
    ///
    /// Variant mismatch (`finalize` does not match `handle.kind`) is a
    /// programming error and surfaces as `TraceError::Internal`.
    pub async fn end_span(
        &self,
        mut handle: SpanHandle,
        job_id: JobId,
        finalize: SpanFinalize,
        outcome: LifecycleOutcome,
    ) -> Result<()> {
        finalize_span_kind(&mut handle.kind, finalize, &handle.span_id)?;
        if let SpanKind::LlmCall {
            begin,
            result: Some(result),
        } = &handle.kind
        {
            self.stream.publish(TraceEvent::LlmSpanEnded {
                span_id: handle.span_id,
                job_id,
                session_id: self.session_id.clone(),
                user_id: self.user_id.clone(),
                model_id: begin.model_id.clone(),
                provider: begin.provider.clone(),
                input_tokens: result.input_tokens,
                output_tokens: result.output_tokens,
            });
        }
        let span = Span {
            id: handle.span_id,
            step_id: handle.step_id,
            kind: handle.kind,
            parallel_group: handle.parallel_group,
            started_at: handle.started_at,
            ended_at: Some(Utc::now()),
            outcome: outcome.clone(),
            events: Vec::new(),
        };
        self.trace_store.save_span(&span).await?;
        self.stream.publish(TraceEvent::SpanEnded {
            span_id: handle.span_id,
            step_id: handle.step_id,
            job_id,
            outcome,
        });
        Ok(())
    }

    // ── SpanEvent ────────────────────────────────────────────────

    /// Close a half-open span without supplying end-time data. Used
    /// by `?`-error paths that need to release a half-open span
    /// quickly without constructing a meaningful result; the stored
    /// `SpanKind`'s `result` field stays `None` (or whatever it was
    /// at begin time).
    pub async fn cancel_span(
        &self,
        handle: SpanHandle,
        job_id: JobId,
        outcome: LifecycleOutcome,
    ) -> Result<()> {
        self.end_span(handle, job_id, SpanFinalize::Empty, outcome)
            .await
    }

    /// Emit a SpanEvent (sanitize hit / approval).
    /// `seq` is caller-supplied because the recorder does not own
    /// per-span sequence counters — each call site that emits
    /// multiple events on the same span manages its own counter.
    pub async fn emit_event(&self, span_id: SpanId, seq: u32, kind: SpanEventKind) -> Result<()> {
        let event = SpanEvent::new(span_id, seq, kind);
        self.trace_store.append_span_event(&event).await?;
        self.stream.publish(TraceEvent::SpanEventEmitted(event));
        Ok(())
    }
}

/// Apply a [`SpanFinalize`] payload to the begin-time `SpanKind` in
/// place. Returns `Err(TraceError::Internal)` when the finalize variant
/// does not match the kind variant — that's a caller bug. `Empty` is
/// always allowed (used by `cancel_span`).
fn finalize_span_kind(kind: &mut SpanKind, finalize: SpanFinalize, span_id: &SpanId) -> Result<()> {
    match (kind, finalize) {
        (SpanKind::LlmCall { result, .. }, SpanFinalize::LlmCall(r)) => {
            *result = Some(r);
            Ok(())
        }
        (SpanKind::ToolCall { result, .. }, SpanFinalize::ToolCall(r)) => {
            *result = Some(r);
            Ok(())
        }
        (_, SpanFinalize::Empty) => Ok(()),
        _ => Err(TraceError::Internal(anyhow::anyhow!(
            "span {} end_span finalize variant does not match begin-time kind",
            span_id
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_storage::test_support::MemoryTraceStore;

    fn make_recorder() -> SpanRecorder {
        SpanRecorder::new(
            SessionId::from("cli-test"),
            "user-test".to_string(),
            Arc::new(MemoryTraceStore::new()),
        )
    }

    fn dummy_llm_kind() -> SpanKind {
        SpanKind::LlmCall {
            begin: aura_trace::LlmCallBegin {
                model_id: "claude".into(),
                provider: "anthropic".into(),
                provider_config_hash: "h".into(),
                input_messages: vec![],
                temperature: None,
            },
            result: None,
        }
    }

    fn llm_finalize(input_tokens: usize, output_tokens: usize) -> SpanFinalize {
        SpanFinalize::LlmCall(aura_trace::LlmCallResult {
            output_content: "hi".into(),
            thinking: None,
            tool_calls: vec![],
            input_tokens,
            output_tokens,
        })
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
    async fn cancel_span_releases_half_open_without_caller_span_result() {
        // Regression: tool_executor and compress_if_needed used to leak
        // half-open spans on early `?` return paths. cancel_span lets
        // those paths release the span without supplying a finalize
        // payload — `SpanKind`'s `result` stays `None` and `LlmSpanEnded`
        // does NOT fire (a cancelled LLM call has no token counts).
        let rec = make_recorder();
        let mut rx = rec.stream().subscribe();
        let job = JobId::new();
        let step = rec.begin_step(job, StepKind::LlmIteration).await.unwrap();
        let span = rec.begin_span(&step, dummy_llm_kind(), None).await.unwrap();
        rec.cancel_span(
            span,
            job,
            LifecycleOutcome::Failed {
                reason: "sanitize failed".into(),
            },
        )
        .await
        .unwrap();
        let mut tags = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            tags.push(format!("{ev:?}"));
        }
        assert!(tags.iter().any(|t| t.contains("SpanEnded")));
        assert!(
            !tags.iter().any(|t| t.contains("LlmSpanEnded")),
            "cancelled span must not emit a token-count cost event",
        );
    }

    #[tokio::test]
    async fn span_lifecycle_emits_started_and_ended() {
        let rec = make_recorder();
        let mut rx = rec.stream().subscribe();
        let job = JobId::new();
        let step = rec.begin_step(job, StepKind::LlmIteration).await.unwrap();
        let span = rec.begin_span(&step, dummy_llm_kind(), None).await.unwrap();
        rec.end_span(span, job, llm_finalize(10, 5), LifecycleOutcome::Ok)
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
        rec.end_span(span, job, llm_finalize(123, 45), LifecycleOutcome::Ok)
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
    async fn end_span_rejects_variant_mismatch() {
        let rec = make_recorder();
        let job = JobId::new();
        let step = rec.begin_step(job, StepKind::LlmIteration).await.unwrap();
        let span = rec.begin_span(&step, dummy_llm_kind(), None).await.unwrap();
        // Begin-time kind is LlmCall; result variant is ToolCall —
        // programming error, surfaces as TraceError::Internal.
        let err = rec
            .end_span(
                span,
                job,
                SpanFinalize::ToolCall(aura_trace::ToolCallResult {
                    output: serde_json::Value::Null,
                    success: false,
                }),
                LifecycleOutcome::Ok,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, TraceError::Internal(_)));
    }

    #[tokio::test]
    async fn emit_event_publishes_and_persists() {
        let rec = make_recorder();
        let mut rx = rec.stream().subscribe();
        let span = SpanId::new();
        rec.emit_event(
            span,
            0,
            SpanEventKind::Approval {
                decision: aura_model::ApprovalDecision::Approve,
                resource: aura_model::ResourceAccess::ReadFile {
                    path: std::path::PathBuf::from("/tmp/foo"),
                },
            },
        )
        .await
        .unwrap();
        let ev = rx.recv().await.unwrap();
        assert!(matches!(ev, TraceEvent::SpanEventEmitted(_)));
    }
}
