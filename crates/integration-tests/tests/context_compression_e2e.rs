//! End-to-end exercise of the LLM context-compression path.
//!
//! Drives the live `AgentLoop` through `AgentTestHarness` configured
//! with a tight token budget. The harness's default empty
//! `InMemorySummaryLoader` makes the compressor's fast-path stage
//! fall through, so the live LLM-summary stage runs and bills
//! through the same `StubLlm`.
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

use baybo_channels::{AgentEvent, AgentOutput, TurnStatus};
use baybo_integration_tests::AgentTestHarness;
use baybo_llm::{LlmResponse, ModelPricing, StreamEvent, TokenUsage};
use baybo_model::MicroUsd;
use baybo_trace::{CompressionTrigger, SpanKind, StepKind, TraceStore};

const DRAIN_TIMEOUT: Duration = Duration::from_millis(750);

#[tokio::test]
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

    // Find a Compression step in any of the recorded jobs. There
    // should be exactly one across the test run.
    let job_ids: std::collections::BTreeSet<_> = records.iter().map(|r| r.job_id).collect();
    let mut compression_steps = Vec::new();
    for job_id in &job_ids {
        let steps: Vec<baybo_trace::Step> = trace_store
            .list_steps_by_job(job_id)
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
#[tokio::test]
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
        vec![TurnStatus::Compacting, TurnStatus::Compacted],
        "turn 2 must report Compacting then Compacted, got {turn2:?}"
    );

    harness.shutdown().await;
}

fn status_phases(outputs: &[AgentOutput]) -> Vec<TurnStatus> {
    outputs
        .iter()
        .filter_map(|o| match &o.event {
            AgentEvent::Status(s) => Some(*s),
            _ => None,
        })
        .collect()
}
