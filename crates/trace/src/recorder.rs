//! `SpanRecorder` — facade that owns Step / Span / SpanEvent
//! lifecycle for one session, and `TraceEventStream` — the
//! `tokio::sync::broadcast` bus that downstream observers (cost
//! subscriber, TUI / Web UI) subscribe to.
//!
//! See `docs/modules/trace.md` for the design.

use std::collections::HashSet;
use std::sync::Arc;

use baybo_model::{ParallelGroup, SessionId, SpanId, StepId, ToolSetHash, TurnId};
use chrono::Utc;
use parking_lot::Mutex;
use tokio::sync::broadcast;

use crate::{
    LifecycleOutcome, LifecycleState, LlmToolDefinition, LlmToolSet, LlmToolSetRef, Span,
    SpanEvent, SpanEventKind, SpanFinalize, SpanHandle, SpanKind, Step, StepHandle, StepKind,
    TraceError,
};
use baybo_store::TraceStore;

type Result<T> = std::result::Result<T, TraceError>;

const TRACE_EVENT_CHANNEL_CAPACITY: usize = 256;

/// One event published on a session's `TraceEventStream`.
#[derive(Debug, Clone)]
pub enum TraceEvent {
    StepStarted {
        step_id: StepId,
        turn_id: TurnId,
        kind: StepKind,
    },
    StepEnded {
        step_id: StepId,
        turn_id: TurnId,
        outcome: LifecycleOutcome,
    },
    SpanStarted {
        span_id: SpanId,
        step_id: StepId,
        turn_id: TurnId,
        /// String tag of the kind (`"llm_call"`, `"tool_call"`, etc.).
        kind_tag: &'static str,
    },
    SpanEnded {
        span_id: SpanId,
        step_id: StepId,
        turn_id: TurnId,
        outcome: LifecycleOutcome,
    },
    SpanEventEmitted(SpanEvent),
    /// Fired by `end_span` for `SpanKind::LlmCall`. No internal cost
    /// pipeline subscribes to this — `CostManager::record_call` is now
    /// invoked directly by `agent_loop`. The event remains for trace
    /// observers (WebUI live stream, etc.) that want a fan-out signal
    /// when an LLM call closes.
    LlmSpanEnded {
        span_id: SpanId,
        turn_id: TurnId,
        session_id: SessionId,
        user_id: String,
        model_id: String,
        provider: String,
        input_tokens: usize,
        output_tokens: usize,
        cached_input_tokens: usize,
        cache_creation_input_tokens: usize,
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
/// `TurnLifecycle`. Cheap to construct (just three Arc clones + an
/// `mpsc`-style broadcast sender).
pub struct SpanRecorder {
    session_id: SessionId,
    user_id: String,
    trace_store: Arc<dyn TraceStore>,
    stream: TraceEventStream,
    /// Tool sets this recorder has already persisted. The set a session
    /// offers is stable across its calls, so this collapses one store
    /// write per LLM call into one per distinct set.
    saved_tool_sets: Mutex<HashSet<ToolSetHash>>,
}

impl SpanRecorder {
    /// Construct a recorder. `stream` is **required** because trace
    /// observers subscribe to one shared bus per process; a recorder
    /// that publishes into a private bus silently disappears from
    /// downstream listeners. Callers without a process-wide bus
    /// pass `TraceEventStream::new()` explicitly so the choice is on
    /// record at the call site.
    pub fn new(
        session_id: SessionId,
        user_id: String,
        trace_store: Arc<dyn TraceStore>,
        stream: TraceEventStream,
    ) -> Self {
        Self {
            session_id,
            user_id,
            trace_store,
            stream,
            saved_tool_sets: Mutex::new(HashSet::new()),
        }
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn stream(&self) -> &TraceEventStream {
        &self.stream
    }

    /// Persist the tool definitions a call is about to offer the model and
    /// return the reference to put on its `LlmCall` span.
    ///
    /// The set is stored content-addressed and shared by every span that
    /// offered it, so the caller records a hash instead of tens of KB of
    /// schema per call. Repeat sets skip the store entirely — the tool
    /// list is session-stable, so after the first call this is a hash and
    /// a lock.
    pub async fn record_tool_set(&self, tools: Vec<LlmToolDefinition>) -> Result<LlmToolSetRef> {
        let count = tools.len();
        let row = LlmToolSet::new(tools).to_row()?;
        // Two callers can both miss the memo and both write; that costs one
        // redundant no-op INSERT (the row is immutable and keyed by its own
        // digest), which is cheaper than holding a lock across the await.
        let known = self.saved_tool_sets.lock().contains(&row.hash);
        if !known {
            self.trace_store.save_tool_set(&row).await?;
            self.saved_tool_sets.lock().insert(row.hash.clone());
        }
        Ok(LlmToolSetRef {
            hash: row.hash,
            count,
        })
    }

    // ── Step lifecycle ────────────────────────────────────────────

    /// Open a new `Step` under `turn_id`. Persists immediately to the
    /// columnar `steps` table.
    pub async fn begin_step(&self, turn_id: TurnId, kind: StepKind) -> Result<StepHandle> {
        let started_at = Utc::now();
        let step_id = StepId::new();
        let step = Step {
            id: step_id,
            turn_id,
            kind: kind.clone(),
            started_at,
            ended_at: None,
            outcome: LifecycleState::Pending,
        };
        self.trace_store.save_step(&step.to_row()?).await?;
        self.stream.publish(TraceEvent::StepStarted {
            step_id,
            turn_id,
            kind: kind.clone(),
        });
        Ok(StepHandle::new(step_id, turn_id, kind, started_at))
    }

    /// Close a step. Constructs the closed `Step` row from the handle
    /// (which carries the begin-time `kind` + `started_at`) so the
    /// columnar write is a single INSERT OR REPLACE — no SELECT.
    pub async fn end_step(&self, handle: StepHandle, outcome: LifecycleOutcome) -> Result<()> {
        let step = Step {
            id: handle.step_id,
            turn_id: handle.turn_id,
            kind: handle.kind,
            started_at: handle.started_at,
            ended_at: Some(Utc::now()),
            outcome: LifecycleState::Done(outcome.clone()),
        };
        self.trace_store.save_step(&step.to_row()?).await?;
        self.stream.publish(TraceEvent::StepEnded {
            step_id: handle.step_id,
            turn_id: handle.turn_id,
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
            outcome: LifecycleState::Pending,
            events: Vec::new(),
        };
        let kind_tag = kind.tag();
        self.trace_store.save_span(&span.to_row()?).await?;
        self.stream.publish(TraceEvent::SpanStarted {
            span_id,
            step_id: step.step_id,
            turn_id: step.turn_id,
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
        turn_id: TurnId,
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
                turn_id,
                session_id: self.session_id.clone(),
                user_id: self.user_id.clone(),
                model_id: begin.model_id.clone(),
                provider: begin.provider.clone(),
                input_tokens: result.input_tokens,
                output_tokens: result.output_tokens,
                cached_input_tokens: result.cached_input_tokens,
                cache_creation_input_tokens: result.cache_creation_input_tokens,
            });
        }
        let span = Span {
            id: handle.span_id,
            step_id: handle.step_id,
            kind: handle.kind,
            parallel_group: handle.parallel_group,
            started_at: handle.started_at,
            ended_at: Some(Utc::now()),
            outcome: LifecycleState::Done(outcome.clone()),
            events: Vec::new(),
        };
        self.trace_store.save_span(&span.to_row()?).await?;
        self.stream.publish(TraceEvent::SpanEnded {
            span_id: handle.span_id,
            step_id: handle.step_id,
            turn_id,
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
        turn_id: TurnId,
        outcome: LifecycleOutcome,
    ) -> Result<()> {
        self.end_span(handle, turn_id, SpanFinalize::Empty, outcome)
            .await
    }

    /// Emit a SpanEvent (sanitize hit / approval).
    /// `seq` is caller-supplied because the recorder does not own
    /// per-span sequence counters — each call site that emits
    /// multiple events on the same span manages its own counter.
    pub async fn emit_event(&self, span_id: SpanId, seq: u32, kind: SpanEventKind) -> Result<()> {
        let event = SpanEvent::new(span_id, seq, kind);
        self.trace_store.append_span_event(&event.to_row()?).await?;
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
    use crate::test_support::MemoryTraceStore;
    use baybo_store::trace::Result as StoreResult;

    fn make_recorder() -> SpanRecorder {
        SpanRecorder::new(
            SessionId::from("cli-test"),
            "user-test".to_string(),
            Arc::new(MemoryTraceStore::new()),
            TraceEventStream::new(),
        )
    }

    fn dummy_llm_kind() -> SpanKind {
        SpanKind::LlmCall {
            begin: crate::LlmCallBegin {
                model_id: "claude".into(),
                provider: "anthropic".into(),
                reasoning_effort: None,
                input_messages: crate::LlmCallInputs::empty(),
                temperature: None,
                tools: None,
            },
            result: None,
        }
    }

    fn llm_finalize(input_tokens: usize, output_tokens: usize) -> SpanFinalize {
        SpanFinalize::LlmCall(crate::LlmCallResult {
            output_content: "hi".into(),
            thinking: None,
            tool_calls: vec![],
            input_tokens,
            output_tokens,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
        })
    }

    #[tokio::test]
    async fn step_round_trip_emits_events() {
        let rec = make_recorder();
        let mut rx = rec.stream().subscribe();
        let turn = TurnId::new();
        let h = rec.begin_step(turn, StepKind::LlmIteration).await.unwrap();
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
        let turn = TurnId::new();
        let step = rec.begin_step(turn, StepKind::LlmIteration).await.unwrap();
        let span = rec.begin_span(&step, dummy_llm_kind(), None).await.unwrap();
        rec.cancel_span(
            span,
            turn,
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
        let turn = TurnId::new();
        let step = rec.begin_step(turn, StepKind::LlmIteration).await.unwrap();
        let span = rec.begin_span(&step, dummy_llm_kind(), None).await.unwrap();
        rec.end_span(span, turn, llm_finalize(10, 5), LifecycleOutcome::Ok)
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
        let turn = TurnId::new();
        let step = rec.begin_step(turn, StepKind::LlmIteration).await.unwrap();
        let span = rec.begin_span(&step, dummy_llm_kind(), None).await.unwrap();
        rec.end_span(span, turn, llm_finalize(123, 45), LifecycleOutcome::Ok)
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
        let turn = TurnId::new();
        let step = rec.begin_step(turn, StepKind::LlmIteration).await.unwrap();
        let span = rec.begin_span(&step, dummy_llm_kind(), None).await.unwrap();
        // Begin-time kind is LlmCall; result variant is ToolCall —
        // programming error, surfaces as TraceError::Internal.
        let err = rec
            .end_span(
                span,
                turn,
                SpanFinalize::ToolCall(crate::ToolCallResult {
                    output: crate::ToolCallOutput::inline(serde_json::Value::Null),
                    success: false,
                    output_truncated_from: None,
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
                decision: baybo_model::ApprovalDecision::Approve,
                resource: baybo_model::ResourceAccess::ReadFile {
                    path: std::path::PathBuf::from("/tmp/foo"),
                },
            },
        )
        .await
        .unwrap();
        let ev = rx.recv().await.unwrap();
        assert!(matches!(ev, TraceEvent::SpanEventEmitted(_)));
    }

    fn tool(name: &str) -> LlmToolDefinition {
        LlmToolDefinition {
            name: name.into(),
            description: format!("does {name}"),
            parameters_schema: serde_json::json!({ "type": "object" }),
        }
    }

    /// Counts saves so the memo can be observed, and forwards everything
    /// else to a real in-memory store.
    #[derive(Default)]
    struct CountingToolSetStore {
        inner: MemoryTraceStore,
        saves: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl TraceStore for CountingToolSetStore {
        async fn save_step(&self, step: &crate::StepRow) -> StoreResult<()> {
            self.inner.save_step(step).await
        }
        async fn load_step(&self, id: &StepId) -> StoreResult<Option<crate::StepRow>> {
            self.inner.load_step(id).await
        }
        async fn list_steps_by_turn(&self, id: &TurnId) -> StoreResult<Vec<crate::StepRow>> {
            self.inner.list_steps_by_turn(id).await
        }
        async fn list_unfinished_steps(&self) -> StoreResult<Vec<crate::StepRow>> {
            self.inner.list_unfinished_steps().await
        }
        async fn save_span(&self, span: &crate::SpanRow) -> StoreResult<()> {
            self.inner.save_span(span).await
        }
        async fn load_span(&self, id: &SpanId) -> StoreResult<Option<crate::SpanRow>> {
            self.inner.load_span(id).await
        }
        async fn list_spans_by_step(&self, id: &StepId) -> StoreResult<Vec<crate::SpanRow>> {
            self.inner.list_spans_by_step(id).await
        }
        async fn list_spans_by_turn(&self, id: &TurnId) -> StoreResult<Vec<crate::SpanRow>> {
            self.inner.list_spans_by_turn(id).await
        }
        async fn trace_counts_by_turn(&self, id: &TurnId) -> StoreResult<(usize, usize)> {
            self.inner.trace_counts_by_turn(id).await
        }
        async fn save_tool_set(&self, set: &crate::ToolSetRow) -> StoreResult<()> {
            self.saves
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.save_tool_set(set).await
        }
        async fn load_tool_set(
            &self,
            hash: &ToolSetHash,
        ) -> StoreResult<Option<crate::ToolSetRow>> {
            self.inner.load_tool_set(hash).await
        }
        async fn append_span_event(&self, event: &crate::SpanEventRow) -> StoreResult<()> {
            self.inner.append_span_event(event).await
        }
        async fn list_span_events(&self, id: &SpanId) -> StoreResult<Vec<crate::SpanEventRow>> {
            self.inner.list_span_events(id).await
        }
        async fn list_span_events_for_spans(
            &self,
            ids: &[SpanId],
        ) -> StoreResult<Vec<crate::SpanEventRow>> {
            self.inner.list_span_events_for_spans(ids).await
        }
    }

    /// The whole point of the content address: a session that offers the
    /// same tools on every call stores them once. Without the memo this
    /// would write tens of KB per LLM call.
    #[tokio::test]
    async fn a_repeated_tool_set_is_stored_once() {
        let store = Arc::new(CountingToolSetStore::default());
        let rec = SpanRecorder::new(
            SessionId::from("cli-test"),
            "user-test".to_string(),
            Arc::clone(&store) as Arc<dyn TraceStore>,
            TraceEventStream::new(),
        );

        let tools = vec![tool("bash"), tool("read")];
        let first = rec.record_tool_set(tools.clone()).await.unwrap();
        let second = rec.record_tool_set(tools).await.unwrap();

        assert_eq!(first.hash, second.hash);
        assert_eq!(first.count, 2);
        assert_eq!(
            store.saves.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the second call should have been served by the memo"
        );
        let stored = store.load_tool_set(&first.hash).await.unwrap().unwrap();
        assert_eq!(LlmToolSet::from_row(stored).unwrap().tools.len(), 2);
    }

    /// A different tool set is a different row — otherwise a session whose
    /// available tools changed mid-run (a channel switch, a cron fire's
    /// extra tool) would resolve to the wrong list.
    #[tokio::test]
    async fn a_changed_tool_set_gets_its_own_address() {
        let store = Arc::new(CountingToolSetStore::default());
        let rec = SpanRecorder::new(
            SessionId::from("cli-test"),
            "user-test".to_string(),
            Arc::clone(&store) as Arc<dyn TraceStore>,
            TraceEventStream::new(),
        );

        let a = rec.record_tool_set(vec![tool("bash")]).await.unwrap();
        let b = rec
            .record_tool_set(vec![tool("bash"), tool("read")])
            .await
            .unwrap();

        assert_ne!(a.hash, b.hash);
        assert_eq!(store.saves.load(std::sync::atomic::Ordering::Relaxed), 2);
    }
}
