//! End-to-end coverage for the compressor's fast-path stage when it
//! is wired into the live agent loop the way the production runtime
//! wires it.
//!
//! Drives the harness with an `FsSummaryLoader` pointing at a
//! per-test tempdir, pre-seeds a `summary.md` + cursor metadata after
//! the first turn lands real `session_messages` rows, then sends a
//! second turn that crosses the compression budget. The fast-path
//! must skip the live LLM-summary stage entirely — proven by absence
//! of `SUMMARIZE_INSTRUCTION` in any captured `ChatRequest`.

use std::sync::Arc;
use std::time::Duration;

use aura_context::SUMMARIZE_INSTRUCTION;
use aura_integration_tests::AgentTestHarness;
use aura_llm::{StreamEvent, TokenUsage};
use aura_model::ContentBlock;
use chrono::Utc;
use tempfile::TempDir;

const DRAIN_TIMEOUT: Duration = Duration::from_millis(750);

/// Walk every captured request and report whether any of them carries
/// the trailing `SUMMARIZE_INSTRUCTION` user turn — that's the
/// fingerprint of the inner `Summarize` strategy's chat callback
/// firing. Fast-path success means none of the captured requests
/// match; fall-through means at least one does.
fn any_request_invoked_summarizer(requests: &[aura_llm::ChatRequest]) -> bool {
    requests.iter().any(|req| {
        req.messages.last().is_some_and(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text(t) if t.contains(SUMMARIZE_INSTRUCTION)))
        })
    })
}

/// Fast-path wired through the agent loop: after one normal turn lands
/// in the persisted log, we plant a precomputed `summary.md` plus
/// cursor metadata on the bound `SessionManager`. The next turn
/// crosses the compression budget; the wrapper must serve the
/// precomputed summary without invoking the inner Summarize's chat
/// callback. Asserted by inspecting `captured_requests` for the
/// summarizer instruction fingerprint.
#[tokio::test]
async fn fast_path_skips_chat_callback_when_summary_md_present() {
    // Tempdir is the workspace root. The fast-path will read
    // `<root>/state/sessions/<session_id>/summary.md` from it on the
    // second turn. Held until the test ends.
    let workspace_root = TempDir::new().expect("tempdir");
    let workspace = Arc::new(aura_workspace::WorkspacePaths::new(
        workspace_root.path().to_path_buf(),
    ));

    // Tight budget so the second turn definitely trips the
    // compression gate inside `compress_if_needed`. With max=400 and
    // threshold=0.05 (= 20 tokens), even a short reply pushes us over.
    let mut harness = AgentTestHarness::builder()
        .with_model_context_window(400)
        .with_compression_threshold(0.05)
        .with_workspace(Arc::clone(&workspace))
        .with_keep_recent(2)
        .build();

    // --- Turn 1 ---------------------------------------------------------
    // Push a normal stream response for the first user turn so the
    // agent loop has something to send back. No compression should
    // run yet (no metadata, no summary.md).
    let usage = TokenUsage {
        input_tokens: 50,
        output_tokens: 20,
        cached_input_tokens: 0,
        cache_creation_input_tokens: 0,
    };
    harness.stub_llm.push_stream_results(vec![
        Ok(StreamEvent::Text("first reply".into())),
        Ok(StreamEvent::Usage(usage)),
    ]);
    harness.send_text("hello").await.unwrap();
    let _ = harness.drain_outputs(DRAIN_TIMEOUT).await;

    // --- Plant the precomputed summary state ---------------------------
    // After turn 1 the persisted active log contains the system
    // prompt (ordinal=1), the user message (=2) and the assistant
    // reply (=3). Cursor=2 places the wrapper's coverage boundary at
    // the user message so the post-cursor span is exactly the
    // assistant reply plus whatever turn 2 appends.
    let summary_path = workspace.session_summary_file(harness.session.id.as_str());
    tokio::fs::create_dir_all(summary_path.parent().expect("summary dir"))
        .await
        .expect("session summary dir");
    tokio::fs::write(&summary_path, "PRECOMPUTED SUMMARY OF EARLIER CONVERSATION")
        .await
        .expect("write summary.md");
    harness
        .session_manager
        .record_summary_success(
            &harness.session.id,
            2,
            1_000,
            "stub-model",
            "span-pre",
            Utc::now(),
        )
        .await
        .expect("seed summary metadata");

    // Snapshot the captured-requests count so we can isolate turn 2's
    // additions and assert about them in particular.
    let pre_turn2_request_count = harness.stub_llm.captured_requests().len();

    // --- Turn 2 ---------------------------------------------------------
    // Push the next main-turn stream response. We do NOT push a
    // compression chat response — if the wrapper falls through to the
    // inner Summarize, that strategy will call `chat()` and capture
    // the SUMMARIZE_INSTRUCTION fingerprint on the request even though
    // the queue is empty. Either way, captured_requests carries the
    // signal; the chat queue's emptiness simply gates whether
    // Summarize fell back.
    harness.stub_llm.push_stream_results(vec![
        Ok(StreamEvent::Text("second reply".into())),
        Ok(StreamEvent::Usage(usage)),
    ]);
    harness.send_text("again").await.unwrap();
    let _ = harness.drain_outputs(DRAIN_TIMEOUT).await;

    // --- Assertions ----------------------------------------------------
    let captured = harness.stub_llm.captured_requests();
    assert!(
        captured.len() > pre_turn2_request_count,
        "turn 2 must have produced at least one captured request"
    );
    assert!(
        !any_request_invoked_summarizer(&captured),
        "fast-path must skip the inner Summarize's chat callback; \
         captured a request whose trailing message carries SUMMARIZE_INSTRUCTION: {captured:#?}"
    );

    // The harness's `session_summaries.in_flight` flag stays clear
    // throughout: the wrapper never spawns a refresh pass, and
    // `record_summary_success` we ran above wrote `in_flight = 0`.
    let row = harness
        .session_manager
        .summary_metadata(&harness.session.id)
        .await
        .unwrap()
        .expect("metadata exists");
    assert!(
        !row.in_flight,
        "fast-path read path must not flip the in_flight flag"
    );

    harness.shutdown().await;
}
