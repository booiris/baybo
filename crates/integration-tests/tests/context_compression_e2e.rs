//! End-to-end exercise of the LLM context-compression path.
//!
//! Drives the live `AgentLoop` through `AgentTestHarness` configured
//! with a tight token budget and the `Summarize` strategy backed by a
//! second `StubLlm` that scripts the summary. Asserts:
//!
//!  1. The agent runs two consecutive turns (compression fires before
//!     the second turn's LLM call).
//!  2. The cost ledger contains both main-call and compression-call
//!     `CostRecord`s.
//!  3. The compression cost row's `span_id` matches the
//!     `SpanKind::LlmCall` span recorded under a `StepKind::Compression`
//!     step in the trace store — i.e. the cost row joins back to the
//!     trace span for any future "click cost row → trace span" UI.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use aura_agent::compression::LlmSummarizer;
use aura_context::{CompressionStrategy, Summarize, budget::TokenBudget};
use aura_integration_tests::AgentTestHarness;
use aura_llm::test_support::StubLlm;
use aura_llm::{LlmCompletion, LlmResponse, ModelInfo, ModelPricing, StreamEvent, TokenUsage};
use aura_model::MicroUsd;
use aura_storage::TraceStore;
use aura_trace::{SpanKind, StepKind};

const DRAIN_TIMEOUT: Duration = Duration::from_millis(750);

#[tokio::test]
async fn compression_call_records_cost_with_matching_span_id() {
    // ── Pricing: nonzero so `record_call` actually moves the meter
    // and persists. Both the main and the summarizer model have to
    // appear in the pricing map; otherwise `compute_cost_usd` returns
    // zero, the `if cost > 0 { … }` branch is skipped on the meter
    // (still persisted), but the test assertions on token counts
    // still hold either way.
    let main_model = "stub-model";
    let summarizer_model = "summarizer-model";
    let mut pricing_map: HashMap<String, ModelPricing> = HashMap::new();
    let pricing_entry = ModelPricing {
        input_per_1m_tokens: MicroUsd::from_usd_decimal(3.0),
        output_per_1m_tokens: MicroUsd::from_usd_decimal(15.0),
    };
    pricing_map.insert(main_model.into(), pricing_entry);
    pricing_map.insert(summarizer_model.into(), pricing_entry);
    let pricing = Arc::new(pricing_map);

    // ── Summarizer LLM: separate `StubLlm` so its model_id is
    // distinct from the agent's main model — lets us join the cost
    // record back to the compression call by `model` field.
    let summarizer_stub = Arc::new(StubLlm::new().with_model_info(ModelInfo {
        id: summarizer_model.into(),
        provider: "summarizer-provider".into(),
        context_window: 8_192,
        supports_tools: false,
        supports_vision: false,
        pricing: pricing_entry,
    }));
    summarizer_stub.push_response(LlmResponse {
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

    let strategy: Box<dyn CompressionStrategy> = Box::new(Summarize::new(
        Arc::new(LlmSummarizer::new(
            summarizer_stub.clone() as Arc<dyn LlmCompletion>
        )),
        1, // keep_recent
    ));

    // ── Tight budget: max=200 tokens, threshold=0.1 → any meaningful
    // turn easily crosses the gate so `compress_if_needed` fires
    // before turn 2's LLM call.
    let mut harness = AgentTestHarness::builder()
        .with_pricing(pricing)
        .with_token_budget(TokenBudget::new(200, 0.1))
        .with_compression_strategy(strategy)
        .build();

    // ── Main-loop scripts: each turn streams a small text plus a
    // Usage event so `record_llm_call` persists a cost row.
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

    // ── Drive two turns. The second turn's `compress_if_needed`
    // should pull in the summarizer.
    harness.send_text("hello").await.unwrap();
    let _ = harness.drain_outputs(DRAIN_TIMEOUT).await;
    harness.send_text("again").await.unwrap();
    let _ = harness.drain_outputs(DRAIN_TIMEOUT).await;

    // ── Assertions.
    let records = harness.cost_store.records();
    assert!(
        records.len() >= 3,
        "expected at least 3 cost records (turn1, compression, turn2); got: {records:#?}"
    );

    let compression_records: Vec<_> = records
        .iter()
        .filter(|r| r.model == summarizer_model)
        .collect();
    assert_eq!(
        compression_records.len(),
        1,
        "exactly one compression cost row should be persisted; got: {compression_records:#?}"
    );
    let compression_record = compression_records[0];
    assert_eq!(compression_record.input_tokens, 250);
    assert_eq!(compression_record.output_tokens, 40);

    let main_records: Vec<_> = records.iter().filter(|r| r.model == main_model).collect();
    assert_eq!(
        main_records.len(),
        2,
        "expected two main-call cost rows (turn1, turn2); got: {main_records:#?}"
    );

    // ── Trace-side: locate the `Compression` step containing the
    // `LlmCall` span and assert the cost row's `span_id` matches.
    let trace_store: Arc<dyn TraceStore> = harness.trace_store.clone();
    // The compression record carries the matching job_id so we can
    // skip the job-store enumeration entirely.
    let compression_job = compression_record.job_id;
    let steps = trace_store
        .list_steps_by_job(&compression_job)
        .await
        .unwrap();
    let compression_step = steps
        .iter()
        .find(|s| matches!(s.kind, StepKind::Compression))
        .expect("a `StepKind::Compression` step exists for the compression job");
    let spans = trace_store
        .list_spans_by_step(&compression_step.id)
        .await
        .unwrap();
    let llm_span = spans
        .iter()
        .find(|s| matches!(s.kind, SpanKind::LlmCall { .. }))
        .expect("the compression step contains an `LlmCall` span");

    assert_eq!(
        llm_span.id, compression_record.span_id,
        "cost record's span_id must match the trace span's id so the join key is preserved"
    );

    harness.shutdown().await;
}
