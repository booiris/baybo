//! End-to-end exercise of the in-actor background-summary pass +
//! the FS orphan reaper.
//!
//! These tests stand up a sqlite-backed `Store` and a real
//! `SessionManager` with the `SessionSummaryStore` wired in, then
//! drive:
//!
//!  1. `record_summary_success` → `summary_metadata` round trip
//!     across the `SessionManager` API.
//!  2. A full background-summary pass via `baybo_context::run_background_summary`
//!     (the same flow the in-actor `BackgroundCompressionRunner` delegates to):
//!     it writes `summary.md`, advances `session_summaries.cursor`, and
//!     creates no extra session — the pass runs against the parent's storage.
//!  3. The inline fast-path's two on-disk inputs (`summary.md` +
//!     `session_summaries.cursor`) are present after a pass.
//!  4. `reap_orphan_summaries`: an orphan summary directory with no
//!     metadata row is removed; a known one survives.
//!
//! The LLM call / JobLifecycle / SpanRecorder wrapping the pass is
//! exercised at the `baybo-agent` unit layer; this file focuses on the
//! storage + filesystem boundary and the parent-only-session
//! invariant.

use std::sync::Arc;
use std::time::Duration;

use baybo_agent::SessionManager;
use baybo_agent::compression::reap_orphan_summaries;
use baybo_context::{
    BackgroundSummaryConfig, ContextError, SummaryChatRun, TiktokenTokenizer,
    run_background_summary,
};
use baybo_integration_tests::AgentTestHarness;
use baybo_job::{Job, JobStore};
use baybo_llm::{LlmResponse, StreamEvent, TokenUsage, ToolCallInfo};
use baybo_model::{
    ChannelType, ChatMessage, ContentBlock, Session, SessionId, SessionState, TriggerSource, User,
};
use baybo_storage::Store;
use baybo_trace::{Step, StepKind, TraceStore};
use baybo_workspace::WorkspacePaths;
use chrono::Utc;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

fn user(id: &str) -> User {
    User {
        id: id.to_string(),
        name: None,
        channel: ChannelType::tui(),
    }
}

fn root_session(id: &str) -> Session {
    let now = Utc::now();
    Session {
        id: SessionId::from(id),
        user: user(&format!("u-{id}")),
        channel: ChannelType::tui(),
        created_at: now,
        last_active: now,
        state: SessionState::default(),
        root_session_id: SessionId::from(id),
        trigger: TriggerSource::User,
        lineage: None,
        hidden: false,
        pinned: false,
        archived: false,
        folder_id: None,
        title: None,
    }
}

async fn fresh_store_and_paths() -> (Store, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("storage.db");
    let store = Store::open(&db_path).await.expect("open store");
    (store, dir)
}

/// A `summary.md` body line unique to the seeded scaffold. The fake
/// LLM `Edit`-replaces it so the test can assert the file changed.
const SEEDED_WORKLOG_MARKER: &str =
    "_Step by step, what was attempted, done? Very terse summary for each step_";
const PASS_SUMMARY_TEXT: &str = "user wired the background-summary pass into the parent actor";

/// Round-trip a successful summary pass through the `SessionManager`
/// wrapper layer — the same surface the background pass uses.
#[tokio::test]
async fn record_then_read_summary_metadata_via_session_manager() {
    let (store, _dir) = fresh_store_and_paths().await;
    let session = root_session("user-A");
    store.session.save(&session).await.unwrap();

    let mgr = SessionManager::new(
        store.session.clone(),
        store.session_summary.clone(),
        store.session_folder.clone(),
    );

    // Initially no metadata.
    let none = mgr.summary_metadata(&session.id).await.unwrap();
    assert!(none.is_none());

    let now = Utc::now();
    mgr.record_summary_success(&session.id, 42, 12_345, "claude-opus-4-7", "span-1", now)
        .await
        .unwrap();

    let row = mgr.summary_metadata(&session.id).await.unwrap().unwrap();
    assert_eq!(row.cursor, 42);
    assert_eq!(row.pass_count, 1);
    assert_eq!(row.cost_micros, 12_345);
    assert_eq!(row.model_id, "claude-opus-4-7");
    assert_eq!(row.span_id, "span-1");
    assert_eq!(row.error_count, 0);

    // Failure bumps error_count and accumulates spend, without touching
    // cursor / pass_count.
    mgr.record_summary_failure(&session.id, 55, "model", "span-err", Utc::now())
        .await
        .unwrap();
    let row = mgr.summary_metadata(&session.id).await.unwrap().unwrap();
    assert_eq!(row.error_count, 1);
    assert_eq!(row.cursor, 42);
    assert_eq!(row.pass_count, 1);
    assert_eq!(
        row.cost_micros, 12_400,
        "a failed attempt still bills what it spent"
    );

    // Successful pass resets error_count.
    mgr.record_summary_success(&session.id, 60, 1, "model", "span-2", Utc::now())
        .await
        .unwrap();
    let row = mgr.summary_metadata(&session.id).await.unwrap().unwrap();
    assert_eq!(row.error_count, 0);
    assert_eq!(row.cursor, 60);
    assert_eq!(row.pass_count, 2);
    assert_eq!(row.cost_micros, 12_401, "12_345 + 55 failed + 1");
}

/// `cursor` advances monotonically. A pass pins `up_to_ordinal` at trigger
/// time but lands its row seconds to minutes later; a compaction inside that
/// window supersedes every row the pass covered and re-points the cursor onto
/// the freshly inserted continuation-summary row. A plain assignment would
/// then drag the cursor *back* onto a superseded ordinal, which
/// `lookup_anchor_index_for_cursor` cannot resolve — so `tokens_since_anchor`
/// reads the whole transcript, the diff gate is satisfied forever, and the
/// fast path stays dead. 16 of 30 rows in the live install are in exactly
/// that state.
#[tokio::test]
async fn summary_cursor_never_moves_backwards() {
    let (store, _dir) = fresh_store_and_paths().await;
    let session = root_session("user-monotonic");
    store.session.save(&session).await.unwrap();

    let mgr = SessionManager::new(
        store.session.clone(),
        store.session_summary.clone(),
        store.session_folder.clone(),
    );

    // A compaction re-pointed the cursor forward onto the new summary row...
    mgr.record_summary_success(&session.id, 240, 10, "m", "span-repoint", Utc::now())
        .await
        .unwrap();

    // ...and only now does the pass that pinned ordinal 233 finish.
    mgr.record_summary_success(&session.id, 233, 10, "m", "span-late", Utc::now())
        .await
        .unwrap();

    let row = mgr.summary_metadata(&session.id).await.unwrap().unwrap();
    assert_eq!(
        row.cursor, 240,
        "the late pass must not walk the cursor back"
    );
    // Everything else still records the late pass.
    assert_eq!(row.pass_count, 2);
    assert_eq!(row.cost_micros, 20);
    assert_eq!(row.span_id, "span-late");
}

/// Build a deterministic background-summary chat callback that, on its
/// first call, issues one `Edit` rewriting the seeded worklog marker,
/// and on every later call returns a no-tool-call response so the
/// pass terminates. `Arc<AtomicUsize>` tracks the call count so the
/// `FnMut` closure stays `Send`.
fn fake_edit_then_stop(notes_path: std::path::PathBuf) -> baybo_context::BackgroundSummaryCallback {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = Arc::new(AtomicUsize::new(0));
    Box::new(move |_req, _marker| {
        let n = calls.fetch_add(1, Ordering::SeqCst);
        let notes_path = notes_path.clone();
        Box::pin(async move {
            let response = if n == 0 {
                LlmResponse {
                    content: String::new(),
                    content_blocks: vec![],
                    tool_calls: vec![ToolCallInfo {
                        id: "edit-1".into(),
                        name: "Edit".into(),
                        arguments: serde_json::json!({
                            "file_path": notes_path.display().to_string(),
                            "old_string": SEEDED_WORKLOG_MARKER,
                            "new_string": PASS_SUMMARY_TEXT,
                        }),
                        signature: None,
                    }],
                    usage: TokenUsage::default(),
                    thinking: None,
                }
            } else {
                LlmResponse {
                    content: "done".into(),
                    content_blocks: vec![ContentBlock::Text("done".into())],
                    tool_calls: vec![],
                    usage: TokenUsage::default(),
                    thinking: None,
                }
            };
            Ok(SummaryChatRun {
                response,
                span_id: format!("span-{n}"),
                cost_micros: 100,
            })
        })
    })
}

/// The core assertion: a background pass run inline writes `summary.md`,
/// advances `session_summaries.cursor` to the pinned ordinal, and creates
/// no extra session row. Afterwards the inline fast-path's two on-disk
/// inputs are both present.
#[tokio::test]
async fn background_pass_writes_summary_and_advances_cursor_without_extra_session() {
    let (store, dir) = fresh_store_and_paths().await;
    let parent = root_session("parent-inline");
    store.session.save(&parent).await.unwrap();

    let mgr = Arc::new(SessionManager::new(
        store.session.clone(),
        store.session_summary.clone(),
        store.session_folder.clone(),
    ));

    // Two persisted parent turns so `load_active_session_messages_up_to`
    // returns a non-empty transcript for the pass to summarize.
    let o1 = mgr
        .append_session_message(
            &parent.id,
            &ChatMessage::user(vec![ContentBlock::Text("how do I wire the pass?".into())]),
        )
        .await
        .unwrap();
    let up_to_ordinal = mgr
        .append_session_message(
            &parent.id,
            &ChatMessage::assistant(vec![ContentBlock::Text("spawn it detached".into())]),
        )
        .await
        .unwrap();
    assert!(up_to_ordinal > o1);

    let paths = WorkspacePaths::new(dir.path().join("workspace"));
    let notes_path = paths.session_summary_file(parent.id.as_str());

    let config = BackgroundSummaryConfig {
        workspace: Arc::new(paths.clone()),
        sessions: Arc::clone(&mgr),
        tokenizer: Arc::new(TiktokenTokenizer::for_model("test")),
        session_id: parent.id.clone(),
        up_to_ordinal,
        model_id: "test-model".into(),
        cancel_token: CancellationToken::new(),
    };
    let outcome = run_background_summary(config, fake_edit_then_stop(notes_path.clone()))
        .await
        .expect("background summary pass should succeed");

    // Pass outcome pins the cursor at the trigger-time ordinal.
    assert_eq!(outcome.cursor, up_to_ordinal);

    // summary.md was written and reflects the Edit the pass applied.
    let body = tokio::fs::read_to_string(&notes_path)
        .await
        .expect("summary.md must exist after the pass");
    assert!(
        body.contains(PASS_SUMMARY_TEXT),
        "summary.md must contain the edited text, got:\n{body}"
    );
    assert!(
        !body.contains(SEEDED_WORKLOG_MARKER),
        "the seeded marker must have been replaced"
    );

    // session_summaries.cursor advanced to the pinned ordinal.
    let meta = mgr.summary_metadata(&parent.id).await.unwrap().unwrap();
    assert_eq!(meta.cursor, up_to_ordinal);
    assert_eq!(meta.pass_count, 1);

    // The pass created NO extra session — the whole point of the
    // in-actor model. Only the parent row exists, and it has no
    // lineage children.
    let all = store.session.list_all().await.unwrap();
    assert_eq!(
        all.len(),
        1,
        "in-actor background pass must NOT create a separate session"
    );
    assert_eq!(all[0].id, parent.id);
    assert!(
        store
            .session
            .list_lineage_children(&parent.id)
            .await
            .unwrap()
            .is_empty(),
        "in-actor background pass must NOT spawn a child session"
    );

    // The inline fast-path reads exactly these two on-disk inputs; both
    // are now present, so a subsequent turn's fast-path can hit.
    assert!(notes_path.exists(), "fast-path reads summary.md");
    assert!(
        mgr.summary_metadata(&parent.id).await.unwrap().is_some(),
        "fast-path reads session_summaries.cursor"
    );
}

/// Build a callback modelling a model that lands one real `Edit`, then
/// never terminates: every later call re-issues an `Edit` against the
/// already-replaced marker, so the real `EditTool` errors (`ERROR:`
/// tool_result) and the round makes no progress. Counts calls so the test
/// can assert the pass bailed early instead of re-sending the transcript up
/// to the hard iteration cap.
fn fake_edit_then_thrash(
    notes_path: std::path::PathBuf,
    calls: Arc<std::sync::atomic::AtomicUsize>,
) -> baybo_context::BackgroundSummaryCallback {
    use std::sync::atomic::Ordering;
    Box::new(move |_req, _marker| {
        let n = calls.fetch_add(1, Ordering::SeqCst);
        let notes_path = notes_path.clone();
        Box::pin(async move {
            // Round 0 replaces the seeded marker; every later round tries to
            // replace it again — it is gone, so the Edit fails and the round
            // is unproductive.
            let new_string = if n == 0 {
                PASS_SUMMARY_TEXT
            } else {
                "irrelevant"
            };
            let mut tool_calls = vec![ToolCallInfo {
                id: format!("edit-{n}"),
                name: "Edit".into(),
                arguments: serde_json::json!({
                    "file_path": notes_path.display().to_string(),
                    "old_string": SEEDED_WORKLOG_MARKER,
                    "new_string": new_string,
                }),
                signature: None,
            }];
            if n == 0 {
                // Pair the landing edit with a Read so the round isn't
                // "every call a clean Edit" — otherwise the converge
                // short-circuit ends the pass here and the thrash rounds this
                // test exists to exercise never run.
                tool_calls.push(ToolCallInfo {
                    id: "read-0".into(),
                    name: "Read".into(),
                    arguments: serde_json::json!({
                        "file_path": notes_path.display().to_string(),
                    }),
                    signature: None,
                });
            }
            let response = LlmResponse {
                content: String::new(),
                content_blocks: vec![],
                tool_calls,
                usage: TokenUsage::default(),
                thinking: None,
            };
            Ok(SummaryChatRun {
                response,
                span_id: format!("span-{n}"),
                cost_micros: 100,
            })
        })
    })
}

/// A model whose every `Edit` fails: no round ever lands a change, so
/// `summary.md` is still the untouched scaffold when the pass gives up.
fn fake_always_failing_edit(
    notes_path: std::path::PathBuf,
    calls: Arc<std::sync::atomic::AtomicUsize>,
) -> baybo_context::BackgroundSummaryCallback {
    use std::sync::atomic::Ordering;
    Box::new(move |_req, _marker| {
        let n = calls.fetch_add(1, Ordering::SeqCst);
        let notes_path = notes_path.clone();
        Box::pin(async move {
            let response = LlmResponse {
                content: String::new(),
                content_blocks: vec![],
                tool_calls: vec![ToolCallInfo {
                    id: format!("edit-{n}"),
                    name: "Edit".into(),
                    arguments: serde_json::json!({
                        "file_path": notes_path.display().to_string(),
                        "old_string": "text that is not anywhere in the notes file",
                        "new_string": "never lands",
                    }),
                    signature: None,
                }],
                usage: TokenUsage::default(),
                thinking: None,
            };
            Ok(SummaryChatRun {
                response,
                span_id: format!("span-{n}"),
                cost_micros: 100,
            })
        })
    })
}

/// A round where every call was an `Edit` and every one applied ends the pass
/// immediately. The prompt asks for exactly that shape ("make all Edit tool
/// calls in parallel in a single message", "stop"), and the only thing another
/// turn can return is the empty tool-call reply that would break the loop —
/// which costs a full ~145K-token transcript re-send to receive ~500 output
/// tokens. Across the live install that confirmation turn was 33% of all
/// background-summary input for 8% of the output.
#[tokio::test]
async fn background_pass_stops_without_a_confirmation_turn_when_every_edit_lands() {
    let (store, dir) = fresh_store_and_paths().await;
    let parent = root_session("parent-clean-edits");
    store.session.save(&parent).await.unwrap();

    let mgr = Arc::new(SessionManager::new(
        store.session.clone(),
        store.session_summary.clone(),
        store.session_folder.clone(),
    ));

    let up_to_ordinal = mgr
        .append_session_message(
            &parent.id,
            &ChatMessage::user(vec![ContentBlock::Text("summarize this".into())]),
        )
        .await
        .unwrap();

    let paths = WorkspacePaths::new(dir.path().join("workspace"));
    let notes_path = paths.session_summary_file(parent.id.as_str());

    let config = BackgroundSummaryConfig {
        workspace: Arc::new(paths.clone()),
        sessions: Arc::clone(&mgr),
        tokenizer: Arc::new(TiktokenTokenizer::for_model("test")),
        session_id: parent.id.clone(),
        up_to_ordinal,
        model_id: "test-model".into(),
        cancel_token: CancellationToken::new(),
    };

    // `fake_edit_then_stop` would happily serve a second turn; the pass must
    // not ask for one.
    let outcome = run_background_summary(config, fake_edit_then_stop(notes_path.clone()))
        .await
        .expect("a landed edit is a successful pass");

    assert_eq!(outcome.cursor, up_to_ordinal);
    let body = tokio::fs::read_to_string(&notes_path).await.unwrap();
    assert!(
        body.contains(PASS_SUMMARY_TEXT),
        "the edit must have landed"
    );
    let meta = mgr.summary_metadata(&parent.id).await.unwrap().unwrap();
    assert_eq!(meta.pass_count, 1);
    assert_eq!(
        outcome.cost_micros, 100,
        "exactly one billed call — a second would double this"
    );
}

/// Once the model has attempted an `Edit` it has already composed what it
/// wants to write, so retry rounds drop the transcript from the wire: the
/// content lives in the model's own prior turn, and the file's current bytes
/// can only come from a fresh `Read` anyway. The pinned transcript is the
/// entire cost of the pass, and it was being re-sent verbatim every iteration.
#[tokio::test]
async fn retry_rounds_do_not_resend_the_transcript() {
    let (store, dir) = fresh_store_and_paths().await;
    let parent = root_session("parent-elide");
    store.session.save(&parent).await.unwrap();

    let mgr = Arc::new(SessionManager::new(
        store.session.clone(),
        store.session_summary.clone(),
        store.session_folder.clone(),
    ));

    let marker = "UNIQUE-TRANSCRIPT-MARKER-9f3a";
    let mut up_to_ordinal = 0;
    for i in 0..3 {
        up_to_ordinal = mgr
            .append_session_message(
                &parent.id,
                &ChatMessage::user(vec![ContentBlock::Text(format!("{marker} turn {i}"))]),
            )
            .await
            .unwrap();
    }

    let paths = WorkspacePaths::new(dir.path().join("workspace"));
    let notes_path = paths.session_summary_file(parent.id.as_str());

    let config = BackgroundSummaryConfig {
        workspace: Arc::new(paths.clone()),
        sessions: Arc::clone(&mgr),
        tokenizer: Arc::new(TiktokenTokenizer::for_model("test")),
        session_id: parent.id.clone(),
        up_to_ordinal,
        model_id: "test-model".into(),
        cancel_token: CancellationToken::new(),
    };

    // Record, per call, whether the transcript marker was on the wire.
    let saw_marker: Arc<parking_lot::Mutex<Vec<bool>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let recorder = Arc::clone(&saw_marker);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let inner = fake_always_failing_edit(notes_path.clone(), Arc::clone(&calls));
    let mut inner = inner;
    let probing: baybo_context::BackgroundSummaryCallback = Box::new(move |req, m| {
        let present = req.messages.iter().any(|msg| {
            msg.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text(t) if t.contains(marker)))
        });
        recorder.lock().push(present);
        inner(req, m)
    });

    let _ = run_background_summary(config, probing).await;

    let seen = saw_marker.lock().clone();
    assert_eq!(seen.len(), 3, "three rounds: one full, two retries");
    assert!(
        seen[0],
        "the first call must carry the transcript — it is the source material"
    );
    assert!(
        !seen[1] && !seen[2],
        "retry rounds must not re-send it, got {seen:?}"
    );
}

/// A pass that lands NO `Edit` must NOT advance the cursor. The notes file is
/// byte-identical to what the pass found, so it still covers only what the
/// previous pass covered; recording success here would let the compressor's
/// fast path later swap a content-free scaffold in for the transcript and
/// silently drop every message in the gap. Observed in production on
/// `a4e1ebf3` (2026-07-22): four consecutive no-edit passes walked the cursor
/// 187 → 223, then a compaction replaced 234 messages with the empty scaffold.
#[tokio::test]
async fn background_pass_that_lands_no_edit_does_not_advance_the_cursor() {
    let (store, dir) = fresh_store_and_paths().await;
    let parent = root_session("parent-no-edit");
    store.session.save(&parent).await.unwrap();

    let mgr = Arc::new(SessionManager::new(
        store.session.clone(),
        store.session_summary.clone(),
        store.session_folder.clone(),
    ));

    let up_to_ordinal = mgr
        .append_session_message(
            &parent.id,
            &ChatMessage::user(vec![ContentBlock::Text("summarize this".into())]),
        )
        .await
        .unwrap();

    let paths = WorkspacePaths::new(dir.path().join("workspace"));
    let notes_path = paths.session_summary_file(parent.id.as_str());

    let config = BackgroundSummaryConfig {
        workspace: Arc::new(paths.clone()),
        sessions: Arc::clone(&mgr),
        tokenizer: Arc::new(TiktokenTokenizer::for_model("test")),
        session_id: parent.id.clone(),
        up_to_ordinal,
        model_id: "test-model".into(),
        cancel_token: CancellationToken::new(),
    };

    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let err = run_background_summary(
        config,
        fake_always_failing_edit(notes_path.clone(), Arc::clone(&calls)),
    )
    .await
    .expect_err("a pass that wrote nothing must not report success");

    assert!(
        matches!(err, ContextError::UnproductiveSummary(_)),
        "the caller backs off on this variant specifically, got: {err:?}"
    );

    // Still bails on MAX_UNPRODUCTIVE_SUMMARY_ROUNDS rather than the hard cap.
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);

    let meta = mgr.summary_metadata(&parent.id).await.unwrap().unwrap();
    assert_eq!(
        meta.cursor, 0,
        "cursor must stay put — summary.md covers nothing"
    );
    assert_eq!(meta.pass_count, 0, "no pass succeeded");
    assert_eq!(meta.error_count, 1, "the wasted pass must be visible");
}

/// A model that keeps calling tools but stops making progress must NOT burn
/// the full iteration cap: the pass bails after `MAX_UNPRODUCTIVE_SUMMARY_ROUNDS`
/// no-progress rounds, keeps the edits that landed, and records the pass as a
/// SUCCESS (cursor advances) — not the hard-cap failure. Regression guard for
/// the runaway background loop that re-sent a ~129K-token transcript ten times.
#[tokio::test]
async fn background_pass_stops_early_when_model_stops_making_progress() {
    let (store, dir) = fresh_store_and_paths().await;
    let parent = root_session("parent-thrash");
    store.session.save(&parent).await.unwrap();

    let mgr = Arc::new(SessionManager::new(
        store.session.clone(),
        store.session_summary.clone(),
        store.session_folder.clone(),
    ));

    let up_to_ordinal = mgr
        .append_session_message(
            &parent.id,
            &ChatMessage::user(vec![ContentBlock::Text("summarize this".into())]),
        )
        .await
        .unwrap();

    let paths = WorkspacePaths::new(dir.path().join("workspace"));
    let notes_path = paths.session_summary_file(parent.id.as_str());

    let config = BackgroundSummaryConfig {
        workspace: Arc::new(paths.clone()),
        sessions: Arc::clone(&mgr),
        tokenizer: Arc::new(TiktokenTokenizer::for_model("test")),
        session_id: parent.id.clone(),
        up_to_ordinal,
        model_id: "test-model".into(),
        cancel_token: CancellationToken::new(),
    };

    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let outcome = run_background_summary(
        config,
        fake_edit_then_thrash(notes_path.clone(), Arc::clone(&calls)),
    )
    .await
    .expect("early-stop must resolve as success, not the iteration-cap error");

    // One productive round + three no-progress rounds (the third trips
    // MAX_UNPRODUCTIVE_SUMMARY_ROUNDS = 3) = 4 LLM calls, far under the
    // 10-iteration hard cap.
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        4,
        "pass must bail after MAX_UNPRODUCTIVE_SUMMARY_ROUNDS, not spin to the cap"
    );

    // The one real Edit is kept and the cursor advanced — a successful pass.
    assert_eq!(outcome.cursor, up_to_ordinal);
    let body = tokio::fs::read_to_string(&notes_path).await.unwrap();
    assert!(
        body.contains(PASS_SUMMARY_TEXT),
        "the landed edit must survive"
    );
    let meta = mgr.summary_metadata(&parent.id).await.unwrap().unwrap();
    assert_eq!(
        meta.pass_count, 1,
        "early-stop records success, not failure"
    );
    assert_eq!(meta.error_count, 0);
}

#[tokio::test]
async fn agent_background_pass_records_compression_step_on_parent_job() {
    let workspace_root = TempDir::new().expect("tempdir");
    let workspace = Arc::new(WorkspacePaths::new(workspace_root.path().to_path_buf()));

    let mut harness = AgentTestHarness::builder()
        .with_workspace(Arc::clone(&workspace))
        .with_background_compression()
        .with_model_context_window(30_000)
        .with_compression_threshold(0.99)
        .build();

    harness.stub_llm.push_stream_results(vec![
        Ok(StreamEvent::Text("done".into())),
        Ok(StreamEvent::Usage(TokenUsage {
            input_tokens: 20_000,
            output_tokens: 10,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
        })),
    ]);
    // The pass drives summary.md entirely through tool calls and typically
    // returns no prose, so an Edit here is the realistic shape — and the only
    // way the span has anything to record.
    let notes_path = workspace.session_summary_file(harness.session.id.as_str());
    harness.stub_llm.push_response(LlmResponse {
        content: String::new(),
        content_blocks: vec![],
        tool_calls: vec![ToolCallInfo {
            id: "edit-span-1".into(),
            name: "Edit".into(),
            arguments: serde_json::json!({
                "file_path": notes_path.display().to_string(),
                "old_string": SEEDED_WORKLOG_MARKER,
                "new_string": PASS_SUMMARY_TEXT,
            }),
            signature: None,
        }],
        usage: TokenUsage {
            input_tokens: 500,
            output_tokens: 20,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
        },
        thinking: None,
    });

    harness.send_text("hello ".repeat(18_000)).await.unwrap();
    let _ = harness.drain_outputs(Duration::from_millis(750)).await;

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if harness
                .session_manager
                .summary_metadata(&harness.session.id)
                .await
                .unwrap()
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("background summary metadata should be written");

    let jobs: Vec<Job> = harness
        .job_store
        .list_by_session(&harness.session.id)
        .await
        .unwrap()
        .into_iter()
        .map(|row| Job::from_row(row).unwrap())
        .collect();
    assert_eq!(
        jobs.len(),
        1,
        "background compression must not create a separate maintenance job"
    );
    let parent_job_id = jobs[0].id;

    let steps: Vec<Step> = harness
        .trace_store
        .list_steps_by_job(&parent_job_id)
        .await
        .unwrap()
        .into_iter()
        .map(|row| Step::from_row(row).unwrap())
        .collect();
    let compression_steps: Vec<_> = steps
        .iter()
        .filter(|step| matches!(step.kind, StepKind::Compression))
        .collect();
    assert_eq!(
        compression_steps.len(),
        1,
        "background compression should render as one Compression step under the parent job; steps: {steps:#?}"
    );

    // The pass is nothing but tool calls, so a span that drops them leaves the
    // trace blank for the most expensive thing the session does off the hot
    // path — a pass could burn ten 130K-token iterations and show nothing.
    let spans: Vec<baybo_trace::Span> = harness
        .trace_store
        .list_spans_by_step(&compression_steps[0].id)
        .await
        .unwrap()
        .into_iter()
        .map(|row| baybo_trace::Span::from_row(row).unwrap())
        .collect();
    let recorded: Vec<_> = spans
        .iter()
        .filter_map(|span| match &span.kind {
            baybo_trace::SpanKind::LlmCall { result, .. } => result.as_ref(),
            _ => None,
        })
        .flat_map(|result| result.tool_calls.iter())
        .collect();
    assert!(
        recorded.iter().any(|tc| tc.name == "Edit"),
        "the Edit the pass issued must reach its span; spans: {spans:#?}"
    );

    harness.shutdown().await;
}

/// FS orphan reaper (now FS-only): an on-disk summary directory whose
/// `session_id` has no `session_summaries` row is removed; a directory
/// whose session HAS a row is preserved. No maintenance-session DB
/// sweep happens anymore.
#[tokio::test]
async fn orphan_reaper_cleans_fs_orphans_only() {
    let (store, dir) = fresh_store_and_paths().await;

    let mgr = Arc::new(SessionManager::new(
        store.session.clone(),
        store.session_summary.clone(),
        store.session_folder.clone(),
    ));

    let paths = WorkspacePaths::new(dir.path().join("workspace"));

    // Orphan: a summary dir with no metadata row.
    let orphan_dir = paths.session_state_dir("orphan-abc");
    tokio::fs::create_dir_all(&orphan_dir).await.unwrap();
    tokio::fs::write(
        paths.session_summary_file("orphan-abc"),
        "stale summary body",
    )
    .await
    .unwrap();
    assert!(paths.session_summary_file("orphan-abc").exists());

    // Known: a summary dir whose session HAS a metadata row.
    let kept_id = "still-known";
    let kept_session = root_session(kept_id);
    store.session.save(&kept_session).await.unwrap();
    mgr.record_summary_success(&kept_session.id, 5, 0, "m", "span", Utc::now())
        .await
        .unwrap();
    let kept_dir = paths.session_state_dir(kept_id);
    tokio::fs::create_dir_all(&kept_dir).await.unwrap();
    tokio::fs::write(paths.session_summary_file(kept_id), "kept body")
        .await
        .unwrap();

    reap_orphan_summaries(&mgr, &paths).await;

    assert!(
        !paths.session_summary_file("orphan-abc").exists(),
        "FS orphan must be deleted"
    );
    assert!(
        paths.session_summary_file(kept_id).exists(),
        "non-orphan summary must survive the sweep"
    );
}

/// The reaper is a no-op when the sessions dir doesn't exist yet (fresh
/// install, first boot before any session wrote a summary).
#[tokio::test]
async fn orphan_reaper_no_op_when_sessions_dir_missing() {
    let (store, dir) = fresh_store_and_paths().await;
    let mgr = Arc::new(SessionManager::new(
        store.session.clone(),
        store.session_summary.clone(),
        store.session_folder.clone(),
    ));
    // Point at a workspace whose state/sessions dir was never created.
    let paths = WorkspacePaths::new(dir.path().join("empty-workspace"));
    // Must not panic / error.
    reap_orphan_summaries(&mgr, &paths).await;
    assert!(!paths.state_sessions_dir().exists());
}

/// Cascade-on-delete: removing a parent session takes its
/// `session_summaries` row with it (`ON DELETE CASCADE` FK).
#[tokio::test]
async fn parent_delete_cascades_summary_metadata() {
    let (store, _dir) = fresh_store_and_paths().await;
    let parent = root_session("parent-cascade");
    store.session.save(&parent).await.unwrap();

    let mgr = SessionManager::new(
        store.session.clone(),
        store.session_summary.clone(),
        store.session_folder.clone(),
    );

    mgr.record_summary_success(&parent.id, 1, 1, "m", "span", Utc::now())
        .await
        .unwrap();
    assert!(mgr.summary_metadata(&parent.id).await.unwrap().is_some());

    mgr.delete(&parent.id).await.unwrap();
    assert!(
        mgr.summary_metadata(&parent.id).await.unwrap().is_none(),
        "summary metadata must cascade-delete with parent"
    );
}

#[tokio::test]
async fn rapid_record_calls_accumulate_cost_and_pass_count() {
    let (store, _dir) = fresh_store_and_paths().await;
    let parent = root_session("rapid");
    store.session.save(&parent).await.unwrap();

    let mgr = SessionManager::new(
        store.session.clone(),
        store.session_summary.clone(),
        store.session_folder.clone(),
    );

    // 5 quick passes — same span_id pattern as production "many
    // refreshes per session" exercise.
    for i in 0..5 {
        mgr.record_summary_success(
            &parent.id,
            i as i64,
            10,
            "m",
            &format!("span-{i}"),
            Utc::now(),
        )
        .await
        .unwrap();
    }
    let row = mgr.summary_metadata(&parent.id).await.unwrap().unwrap();
    assert_eq!(row.pass_count, 5);
    assert_eq!(row.cost_micros, 50);
    assert_eq!(row.cursor, 4);
    assert_eq!(row.span_id, "span-4");

    // Sanity: drain timeout is unrelated; tests run synchronously.
    tokio::time::timeout(Duration::from_millis(10), async {})
        .await
        .unwrap();
}
