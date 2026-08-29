//! End-to-end exercise of the context-compaction path.
//!
//! Drives the live `AgentLoop` through `AgentTestHarness` configured with a
//! tight token budget, so the threshold trips on the second turn and the
//! summariser call bills through the same `StubLlm`.
//!
//! Asserts:
//!  1. The agent runs two consecutive turns; compression fires before
//!     the second turn's main LLM call.
//!  2. The cost ledger contains all three LLM call cost rows
//!     (turn 1 main, compression, turn 2 main).
//!  3. The compression cost row's `span_id` matches the
//!     `SpanKind::LlmCall` span recorded under a `StepKind::Compression`
//!     step in the trace store — the cost row joins back to a real
//!     trace span (real lifecycle, real timing, real input messages),
//!     not the post-hoc placeholder we used to record.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use baybo_channels::{AgentEvent, AgentOutput, StatusPhase};
use baybo_integration_tests::AgentTestHarness;
use baybo_llm::{LlmResponse, ModelPricing, StreamEvent, TokenUsage};
use baybo_model::MicroUsd;
use baybo_trace::{CompressionTrigger, SpanKind, StepKind, TraceStore};

const DRAIN_TIMEOUT: Duration = Duration::from_millis(750);

#[tokio::test(start_paused = true)]
async fn compression_call_records_cost_with_matching_span_id() {
    // Pricing: nonzero so `record_call` actually moves the meter
    // and persists. Only one model id now (the harness's stub) —
    // the summarizer call goes through the same client.
    let model_id = "stub-model";
    let mut pricing_map: HashMap<String, ModelPricing> = HashMap::new();
    pricing_map.insert(
        model_id.into(),
        ModelPricing {
            input_per_1m_tokens: MicroUsd::from_usd_decimal(3.0),
            output_per_1m_tokens: MicroUsd::from_usd_decimal(15.0),
            ..Default::default()
        },
    );
    let pricing = pricing_map;

    // Tight budget: max=200 tokens, threshold=0.1 → any meaningful
    // turn easily crosses the gate so `compress_if_needed` fires
    // before turn 2's LLM call. `keep_recent=1` keeps the pre-flight
    // gate from short-circuiting on the small canned transcript.
    let mut harness = AgentTestHarness::builder()
        .with_pricing(pricing)
        .with_model_context_window(200)
        .with_compression_threshold(0.1)
        .with_keep_recent(1)
        .build();

    // Main-loop scripts: each turn streams a small text plus a Usage
    // event so `record_llm_call` persists a cost row.
    let main_usage = TokenUsage {
        input_tokens: 1_000,
        output_tokens: 50,
        cached_input_tokens: 0,
        cache_creation_input_tokens: 0,
    };
    harness.stub_llm.push_stream_results(vec![
        Ok(StreamEvent::Text("first reply".into())),
        Ok(StreamEvent::Usage(main_usage)),
    ]);
    harness.stub_llm.push_stream_results(vec![
        Ok(StreamEvent::Text("second reply".into())),
        Ok(StreamEvent::Usage(main_usage)),
    ]);

    // Compression call uses non-streaming `chat`. Push a canned
    // response on the same stub.
    harness.stub_llm.push_response(LlmResponse {
        content: "summary of earlier conversation".into(),
        content_blocks: vec![],
        tool_calls: vec![],
        usage: TokenUsage {
            input_tokens: 250,
            output_tokens: 40,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
        },
        thinking: None,
    });

    // Drive two turns. The second turn's `compress_if_needed`
    // should pull the canned summarizer response.
    harness.send_text("hello").await.unwrap();
    let _ = harness.drain_outputs(DRAIN_TIMEOUT).await;
    harness.send_text("again").await.unwrap();
    let _ = harness.drain_outputs(DRAIN_TIMEOUT).await;

    // Locate the Compression step + its LlmCall span; that span_id
    // is the join key the compression cost row must carry.
    let trace_store: Arc<dyn TraceStore> = harness.trace_store.clone();
    let records = harness.cost_store.records();
    assert_eq!(
        records.len(),
        3,
        "expected 3 cost records (turn1, compression, turn2); got: {records:#?}"
    );

    // Find a Compression step in any of the recorded turns. There
    // should be exactly one across the test run.
    let turn_ids: std::collections::BTreeSet<_> = records.iter().map(|r| r.turn_id).collect();
    let mut compression_steps = Vec::new();
    for turn_id in &turn_ids {
        let steps: Vec<baybo_trace::Step> = trace_store
            .list_steps_by_turn(turn_id)
            .await
            .unwrap()
            .into_iter()
            .map(|r| baybo_trace::Step::from_row(r).unwrap())
            .collect();
        compression_steps.extend(
            steps
                .into_iter()
                // The inline (send-time) path is what this suite drives, so
                // assert that trigger rather than any compaction.
                .filter(|s| {
                    matches!(
                        s.kind,
                        StepKind::Compression {
                            trigger: Some(CompressionTrigger::Threshold),
                            ..
                        }
                    )
                }),
        );
    }
    assert_eq!(
        compression_steps.len(),
        1,
        "expected exactly one Compression step across the run; got: {compression_steps:#?}"
    );
    let compression_step = &compression_steps[0];

    let spans: Vec<baybo_trace::Span> = trace_store
        .list_spans_by_step(&compression_step.id)
        .await
        .unwrap()
        .into_iter()
        .map(|r| baybo_trace::Span::from_row(r).unwrap())
        .collect();
    let compression_span = spans
        .iter()
        .find(|s| matches!(s.kind, SpanKind::LlmCall { .. }))
        .expect("the compression step contains an LlmCall span");

    // The matching cost row is the one whose span_id == compression_span.id.
    let compression_record = records
        .iter()
        .find(|r| r.span_id == compression_span.id)
        .expect("a cost record references the compression span_id");
    assert_eq!(compression_record.input_tokens, 250);
    assert_eq!(compression_record.output_tokens, 40);

    // The remaining two rows are the main-call cost rows.
    let main_records: Vec<_> = records
        .iter()
        .filter(|r| r.span_id != compression_span.id)
        .collect();
    assert_eq!(
        main_records.len(),
        2,
        "expected two main-call cost rows besides the compression one"
    );
    for r in &main_records {
        assert_eq!(r.input_tokens, main_usage.input_tokens);
        assert_eq!(r.output_tokens, main_usage.output_tokens);
    }

    // The compression LlmCall span references the summarized transcript
    // prefix by ordinal (`Persisted`) instead of cloning it inline — the
    // span-bloat fix. Only the one-off `SUMMARIZE_INSTRUCTION`, which is
    // not a `session_messages` row, rides inline as the suffix; the
    // transcript itself is recovered from the log at replay time.
    if let SpanKind::LlmCall { begin, .. } = &compression_span.kind {
        match &begin.input_messages {
            baybo_trace::LlmCallInputs::Persisted {
                last_ordinal,
                prefix_len,
                ordinals,
                suffix,
            } => {
                assert!(
                    *last_ordinal >= 0,
                    "compression span must anchor to a real transcript ordinal"
                );
                assert!(
                    *prefix_len >= 1,
                    "compression span must record a real prefix_len tripwire count end-to-end"
                );
                assert!(ordinals.is_empty());
                assert_eq!(
                    suffix.len(),
                    1,
                    "only the summarize instruction rides inline as the suffix"
                );
            }
            other => panic!("expected Persisted, got {other:?}"),
        }
    } else {
        panic!("compression span has unexpected kind");
    }

    harness.shutdown().await;
}

/// The compaction pass reports its phase: turn 2 crosses the tight budget,
/// so the loop emits `Status(Compacting)` before the summariser call and
/// `Status(Compacted)` after; turn 1 stays under threshold and is silent.
#[tokio::test(start_paused = true)]
async fn compaction_reports_compacting_then_compacted_status() {
    let mut harness = AgentTestHarness::builder()
        .with_model_context_window(200)
        .with_compression_threshold(0.1)
        .with_keep_recent(1)
        .build();

    harness
        .stub_llm
        .push_stream(vec![StreamEvent::Text("first".into())]);
    harness
        .stub_llm
        .push_stream(vec![StreamEvent::Text("second".into())]);
    // The turn-2 compression pass's non-streaming summariser call.
    harness.stub_llm.push_response(LlmResponse {
        content: "summary of earlier conversation".into(),
        content_blocks: vec![],
        tool_calls: vec![],
        usage: TokenUsage::default(),
        thinking: None,
    });

    harness.send_text("hello").await.unwrap();
    let turn1 = harness.drain_outputs(DRAIN_TIMEOUT).await;
    assert!(
        status_phases(&turn1).is_empty(),
        "turn 1 is under the budget — no compaction status, got {turn1:?}"
    );

    harness.send_text("again").await.unwrap();
    let turn2 = harness.drain_outputs(DRAIN_TIMEOUT).await;
    assert_eq!(
        status_phases(&turn2),
        vec![StatusPhase::Compacting, StatusPhase::Compacted],
        "turn 2 must report Compacting then Compacted, got {turn2:?}"
    );

    harness.shutdown().await;
}

fn status_phases(outputs: &[AgentOutput]) -> Vec<StatusPhase> {
    outputs
        .iter()
        .filter_map(|o| match &o.event {
            AgentEvent::Status(s) => Some(*s),
            _ => None,
        })
        .collect()
}

/// A summary is the only thing allowed to shorten a conversation. When the
/// summariser call fails the transcript is left exactly as it was — the turn
/// answers anyway — and the user is told, once, that the compaction did not
/// happen. Dropping the middle of the conversation instead was the old
/// behaviour, and it cost a real session its history over a provider blip.
#[tokio::test(start_paused = true)]
async fn a_failed_summariser_keeps_the_transcript_and_warns_the_user() {
    let mut harness = AgentTestHarness::builder()
        .with_model_context_window(200)
        .with_compression_threshold(0.1)
        .with_keep_recent(1)
        .build();

    harness
        .stub_llm
        .push_stream(vec![StreamEvent::Text("first".into())]);
    harness
        .stub_llm
        .push_stream(vec![StreamEvent::Text("second".into())]);
    // Both summariser attempts fail: transient, so the retry is spent too.
    for _ in 0..2 {
        harness
            .stub_llm
            .push_response_err(baybo_llm::LlmError::Transient("summariser down".into()));
    }

    harness.send_text("hello").await.unwrap();
    let _ = harness.drain_outputs(DRAIN_TIMEOUT).await;
    let before = harness
        .session_manager
        .load_active_session_messages(&harness.session.id)
        .await
        .expect("load the active transcript");
    harness.send_text("again").await.unwrap();
    let turn2 = harness.drain_outputs(DRAIN_TIMEOUT).await;

    // A summariser failure must not kill the turn.
    assert!(
        turn2
            .iter()
            .any(|o| matches!(&o.event, AgentEvent::Message(_))),
        "the turn must still answer after the summariser failed, got {turn2:?}"
    );
    assert_eq!(
        status_phases(&turn2),
        vec![StatusPhase::Compacting, StatusPhase::Compacted],
        "the status pair must still bracket a failed compaction"
    );

    // Nothing was dropped to make room: turn 1 is still there in full.
    let after = harness
        .session_manager
        .load_active_session_messages(&harness.session.id)
        .await
        .expect("load the active transcript");
    assert!(
        after.len() >= before.len(),
        "a failed compaction must not shorten the transcript: {before:#?} -> {after:#?}"
    );
    assert!(
        holds_text(&after, "hello"),
        "the opening message must survive a failed compaction: {after:#?}"
    );

    // And the user hears about it — exactly once, not once per iteration.
    let warnings: Vec<&String> = turn2
        .iter()
        .filter_map(|o| match &o.event {
            AgentEvent::Notice {
                level: baybo_channels::NoticeLevel::Warn,
                text,
                ..
            } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(
        warnings.len(),
        1,
        "expected one compaction-failure warning, got {warnings:#?}"
    );
    assert!(
        warnings[0].contains("Context compaction failed"),
        "the warning must name what failed, got {:?}",
        warnings[0]
    );

    // The step says so too: no fallback ran behind it, so it is a failure.
    let trace_store: Arc<dyn TraceStore> = harness.trace_store.clone();
    let mut compression_steps = Vec::new();
    let mut summariser_spans = 0usize;
    for turn_id in harness
        .turn_lifecycle
        .list(None)
        .await
        .unwrap()
        .iter()
        .map(|j| j.id)
    {
        for row in trace_store.list_steps_by_turn(&turn_id).await.unwrap() {
            let step = baybo_trace::Step::from_row(row).unwrap();
            if !matches!(step.kind, StepKind::Compression { .. }) {
                continue;
            }
            summariser_spans += trace_store
                .list_spans_by_step(&step.id)
                .await
                .unwrap()
                .len();
            compression_steps.push(step);
        }
    }
    assert_eq!(
        compression_steps.len(),
        1,
        "expected exactly one compaction step; got {compression_steps:#?}"
    );
    assert!(
        matches!(
            compression_steps[0].outcome,
            baybo_trace::LifecycleState::Done(baybo_trace::LifecycleOutcome::Failed { .. })
        ),
        "an unsummarized compaction is a failure, got {:#?}",
        compression_steps[0].outcome
    );
    assert_eq!(
        summariser_spans, 2,
        "a transient failure is retried once, and both attempts span"
    );

    harness.shutdown().await;
}

/// A non-retriable failure must not spend the retry. A context-window 400 is
/// the likeliest way a compaction fails, and asking again buys the same 400 at
/// full transcript price.
#[tokio::test(start_paused = true)]
async fn a_non_retriable_summariser_failure_is_not_retried() {
    let mut harness = AgentTestHarness::builder()
        .with_model_context_window(200)
        .with_compression_threshold(0.1)
        .with_keep_recent(1)
        .build();

    harness
        .stub_llm
        .push_stream(vec![StreamEvent::Text("first".into())]);
    harness
        .stub_llm
        .push_stream(vec![StreamEvent::Text("second".into())]);
    harness
        .stub_llm
        .push_response_err(baybo_llm::LlmError::BadRequest("context too long".into()));

    harness.send_text("hello").await.unwrap();
    let _ = harness.drain_outputs(DRAIN_TIMEOUT).await;
    harness.send_text("again").await.unwrap();
    let _ = harness.drain_outputs(DRAIN_TIMEOUT).await;

    let trace_store: Arc<dyn TraceStore> = harness.trace_store.clone();
    let mut summariser_spans = 0usize;
    for turn_id in harness
        .turn_lifecycle
        .list(None)
        .await
        .unwrap()
        .iter()
        .map(|j| j.id)
    {
        for row in trace_store.list_steps_by_turn(&turn_id).await.unwrap() {
            let step = baybo_trace::Step::from_row(row).unwrap();
            if !matches!(step.kind, StepKind::Compression { .. }) {
                continue;
            }
            summariser_spans += trace_store
                .list_spans_by_step(&step.id)
                .await
                .unwrap()
                .len();
        }
    }
    assert_eq!(
        summariser_spans, 1,
        "a non-retriable failure must cost exactly one attempt"
    );

    harness.shutdown().await;
}

/// `/stop` aborts an in-flight compaction, and the NEXT turn compacts instead.
///
/// Both halves matter. Cancelling promptly is the point — otherwise the turn
/// cannot unwind until the summariser returns, up to the full read timeout.
/// But the abandoned call must not cost the user their history: the transcript
/// is left exactly as it was, still over budget, so the threshold check at the
/// top of the next turn runs the compaction again. Truncating on a cancel
/// would destroy the middle of the conversation over a `/stop`.
#[tokio::test(start_paused = true)]
async fn a_stop_aborts_the_compaction_and_the_next_turn_redoes_it() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let mut harness = AgentTestHarness::builder()
        .with_model_context_window(4_000)
        .with_compression_threshold(0.65)
        .with_keep_recent(1)
        .with_chat_gate(Arc::clone(&entered), Arc::clone(&release))
        .build();

    // The weight is in turn 1's REPLY, not its prompt: a compaction is decided
    // at the top of a turn, so a heavy prompt would trip the threshold on its
    // own turn and park in the gate before the test is watching. A light
    // prompt with a heavy reply leaves turn 1 under the ceiling and turn 2
    // over it, which puts the first summariser call exactly where the cancel
    // is aimed.
    harness
        .stub_llm
        .push_stream(vec![StreamEvent::Text("padding. ".repeat(1_500))]);
    harness
        .stub_llm
        .push_stream(vec![StreamEvent::Text("after the redo".into())]);
    harness.stub_llm.push_response(LlmResponse {
        content: "<summary>the earlier conversation</summary>".into(),
        content_blocks: vec![],
        tool_calls: vec![],
        usage: TokenUsage::default(),
        thinking: None,
    });

    harness.send_text("hello").await.unwrap();
    let _ = harness.drain_outputs(DRAIN_TIMEOUT).await;
    let before = harness
        .session_manager
        .load_active_session_messages(&harness.session.id)
        .await
        .expect("load the active transcript");

    // Turn 2 trips the threshold, so the summariser call parks in the gate.
    let entered_wait = tokio::spawn({
        let entered = Arc::clone(&entered);
        async move { entered.notified().await }
    });
    tokio::task::yield_now().await;
    harness.send_text("again").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered_wait)
        .await
        .expect("the compaction call should have started")
        .expect("waiter task");

    // Exactly what `handle_stop` does.
    let turns = harness
        .turn_lifecycle
        .list_active_chat_turns_by_session(&harness.session.id)
        .await
        .expect("list active turns");
    assert!(!turns.is_empty(), "a turn must be in flight to cancel");
    for turn in &turns {
        harness
            .turn_lifecycle
            .cancel(&turn.id, baybo_turn::CancelReason::UserStopped, vec![])
            .await
            .expect("cancel the in-flight turn");
    }
    release.notify_waiters();
    let cancelled_turn = harness.drain_outputs(DRAIN_TIMEOUT).await;

    let after_cancel = harness
        .session_manager
        .load_active_session_messages(&harness.session.id)
        .await
        .expect("load the active transcript");
    assert!(
        !holds_summary(&after_cancel),
        "a cancelled compaction must not rewrite the transcript: {after_cancel:#?}"
    );
    assert!(
        after_cancel.len() >= before.len(),
        "nothing may be dropped either — a cancel must not truncate"
    );
    assert!(
        !status_phases(&cancelled_turn).contains(&StatusPhase::Compacted),
        "the compaction was abandoned, so it must not report Compacted"
    );

    // The transcript is still over budget, so the next turn compacts it.
    harness.send_text("once more").await.unwrap();
    let _ = harness.drain_outputs(DRAIN_TIMEOUT).await;
    let after_retry = harness
        .session_manager
        .load_active_session_messages(&harness.session.id)
        .await
        .expect("load the active transcript");
    assert!(
        holds_summary(&after_retry),
        "the next turn must redo the compaction the cancel abandoned: {after_retry:#?}"
    );

    harness.shutdown().await;
}

fn holds_summary(messages: &[baybo_model::ChatMessage]) -> bool {
    holds_text(messages, "the earlier conversation")
}

fn holds_text(messages: &[baybo_model::ChatMessage], needle: &str) -> bool {
    messages.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, baybo_model::ContentBlock::Text(t) if t.contains(needle)))
    })
}
