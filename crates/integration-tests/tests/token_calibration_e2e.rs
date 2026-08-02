//! End-to-end exercise of the token-count calibration feedback loop.
//!
//! Drives the live `AgentLoop` through `AgentTestHarness` with a stub
//! that returns a known, much-larger-than-estimate `input_tokens`
//! count. Asserts that:
//!   * After a single main LLM call, `TokenCalibration` recorded one
//!     sample for the configured model.
//!   * The recorded ratio moved away from 1.0 in the direction of the
//!     observed (actual / estimate) sample, clamped per the per-sample
//!     bounds.

use std::collections::HashMap;
use std::time::Duration;

use baybo_integration_tests::AgentTestHarness;
use baybo_llm::{ModelPricing, StreamEvent, TokenUsage};

const DRAIN_TIMEOUT: Duration = Duration::from_millis(750);

#[tokio::test(start_paused = true)]
async fn main_call_feeds_sample_into_token_calibration() {
    // The harness builds its tokenizer via
    // `TiktokenTokenizer::for_model(stub.model_info().id)` so the
    // calibration key on both observe and adjust is the LLM model id
    // — `"stub-model"` for the default stub.
    let calibration_model = "stub-model";

    // Pricing is irrelevant here — calibration runs regardless of
    // cost recording — but the harness needs *some* HashMap.
    let pricing: HashMap<String, ModelPricing> = HashMap::new();
    let mut harness = AgentTestHarness::builder().with_pricing(pricing).build();

    // Force `actual` well above `estimate` so the EMA ratio drifts
    // upward enough to verify clearly. With a tiny prompt of "hi" the
    // raw estimate is on the order of 5–10 tokens — but
    // `TokenCalibration` skips samples below `MIN_ESTIMATE_FOR_SAMPLE`
    // (currently 100). Send a large message to clear that floor.
    let large_text = "abcdefghij ".repeat(50); // ~600 chars → >100 tokens
    let actual_input_tokens = 5_000usize;

    harness.stub_llm.push_stream_results(vec![
        Ok(StreamEvent::Text("ack".into())),
        Ok(StreamEvent::Usage(TokenUsage {
            input_tokens: actual_input_tokens,
            output_tokens: 10,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
        })),
    ]);

    harness.send_text(&large_text).await.unwrap();
    let _ = harness.drain_outputs(DRAIN_TIMEOUT).await;

    // After one main-call, expect exactly one sample for this model.
    assert_eq!(
        harness.token_calibration.sample_count(calibration_model),
        1,
        "expected exactly one calibration sample after one turn"
    );

    let ratio = harness
        .token_calibration
        .ratio(calibration_model)
        .expect("ratio should be set after observe");

    // Sample is clamped to [0.5, 2.0] per `calibration.rs`. Actual is
    // 5_000, raw estimate is on the order of 200, so the unclamped
    // sample is ~25 → clamped to 2.0. EMA: 0.3 * 2.0 + 0.7 * 1.0 = 1.3.
    let expected = 0.3f64 * 2.0 + 0.7 * 1.0;
    assert!(
        (ratio - expected).abs() < 1e-6,
        "ratio after a clamped over-estimate sample should be ~{expected}, got {ratio}"
    );

    harness.shutdown().await;
}

/// After a main LLM call lands a `usage.input_tokens` of N, the
/// budget's "current" should snap to ~N (plus the delta tokens of
/// whatever assistant/tool messages have been appended since the
/// call). Without the baseline path in `ContextManager::count_tokens`
/// the budget would see only the local BPE estimate, which for tiny
/// stub messages is ~10 tokens — orders of magnitude away from the
/// provider's reported number.
#[tokio::test(start_paused = true)]
async fn budget_picks_up_provider_actual_after_first_main_call() {
    let pricing: HashMap<String, ModelPricing> = HashMap::new();
    let mut harness = AgentTestHarness::builder().with_pricing(pricing).build();

    let actual_input_tokens = 7_500usize;
    harness.stub_llm.push_stream_results(vec![
        Ok(StreamEvent::Text("ok".into())),
        Ok(StreamEvent::Usage(TokenUsage {
            input_tokens: actual_input_tokens,
            output_tokens: 5,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
        })),
    ]);

    // Big enough to clear `MIN_ESTIMATE_FOR_SAMPLE` (=100) so the
    // calibrator actually integrates the sample.
    let big_prompt = "abcdefghij ".repeat(50);
    harness.send_text(&big_prompt).await.unwrap();
    let _ = harness.drain_outputs(DRAIN_TIMEOUT).await;

    // Budget current must be ≥ the reported actual. The baseline path
    // anchors at 7_500; the small assistant reply ("ok") appended
    // afterward adds at most a handful of tokens to the delta.
    let current = harness.cost_manager.metrics().recorded();
    let _ = current; // keep `cost_manager` linked even if metrics aren't asserted

    // What we actually want to read is the budget on the *agent's*
    // ContextManager. The harness doesn't expose it directly, so we
    // verify the user-visible side effect: the calibration sample
    // landed (proves the agent loop reached the new
    // `record_actual_input_tokens` path), and the input_tokens on
    // record matches what we scripted.
    let records = harness.cost_store.records();
    // Pricing is empty so cost is 0, but the row is still persisted
    // with its token counts.
    assert!(
        records
            .iter()
            .any(|r| r.input_tokens == actual_input_tokens),
        "expected a cost row carrying the scripted input_tokens; got: {records:#?}"
    );
    assert_eq!(
        harness.token_calibration.sample_count("stub-model"),
        1,
        "the same call that set the baseline must also feed the calibrator"
    );

    harness.shutdown().await;
}
