//! End-to-end exercise of the in-actor background-summary pass +
//! the FS orphan reaper.
//!
//! These tests stand up a libsql-backed `Store` and a real
//! `SessionManager` with the `SessionSummaryStore` wired in, then
//! drive:
//!
//!  1. `record_summary_success` → `summary_metadata` round trip
//!     across the `SessionManager` API.
//!  2. A full background-summary pass via `aura_context::run_background_summary`
//!     (the same flow the in-actor `BackgroundCompressionRunner` delegates to):
//!     it writes `summary.md`, advances `session_summaries.cursor`, and —
//!     critically for the new model — creates NO maintenance session.
//!  3. The inline fast-path's two on-disk inputs (`summary.md` +
//!     `session_summaries.cursor`) are present after a pass.
//!  4. `reap_maintenance_orphans` — FS-only now: an orphan summary
//!     directory with no metadata row is removed; a known one survives.
//!
//! The LLM call / JobLifecycle / SpanRecorder wrapping the pass is
//! exercised at the `aura-agent` unit layer; this file focuses on the
//! storage + filesystem boundary and the no-maintenance-session
//! invariant.

use std::sync::Arc;
use std::time::Duration;

use aura_agent::SessionManager;
use aura_agent::compression::reap_maintenance_orphans;
use aura_context::{
    BackgroundSummaryConfig, SummaryChatRun, TiktokenTokenizer, run_background_summary,
};
use aura_llm::{LlmResponse, TokenUsage, ToolCallInfo};
use aura_model::{
    ChannelType, ChatMessage, ContentBlock, Session, SessionId, SessionState, TriggerSource, User,
};
use aura_storage::Store;
use aura_workspace::WorkspacePaths;
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
        bound_soul_version: "soul".into(),
        hidden: false,
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

    let mgr = SessionManager::new(store.session.clone(), store.session_summary.clone());

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

    // Failure bumps error_count without touching cursor / pass_count.
    mgr.record_summary_failure(&session.id, "model", "span-err", Utc::now())
        .await
        .unwrap();
    let row = mgr.summary_metadata(&session.id).await.unwrap().unwrap();
    assert_eq!(row.error_count, 1);
    assert_eq!(row.cursor, 42);
    assert_eq!(row.pass_count, 1);

    // Successful pass resets error_count.
    mgr.record_summary_success(&session.id, 60, 1, "model", "span-2", Utc::now())
        .await
        .unwrap();
    let row = mgr.summary_metadata(&session.id).await.unwrap().unwrap();
    assert_eq!(row.error_count, 0);
    assert_eq!(row.cursor, 60);
    assert_eq!(row.pass_count, 2);
    assert_eq!(row.cost_micros, 12_346);
}

/// Build a deterministic background-summary chat callback that, on its
/// first call, issues one `Edit` rewriting the seeded worklog marker,
/// and on every later call returns a no-tool-call response so the
/// pass terminates. `Arc<AtomicUsize>` tracks the call count so the
/// `FnMut` closure stays `Send`.
fn fake_edit_then_stop(notes_path: std::path::PathBuf) -> aura_context::BackgroundSummaryCallback {
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

/// The core new-model assertion: a background pass run inline (no
/// maintenance session, no router hop) writes `summary.md`, advances
/// `session_summaries.cursor` to the pinned ordinal, and leaves NO
/// maintenance session row behind. Afterwards the inline fast-path's
/// two on-disk inputs are both present.
#[tokio::test]
async fn background_pass_writes_summary_and_advances_cursor_without_maintenance_session() {
    let (store, dir) = fresh_store_and_paths().await;
    let parent = root_session("parent-inline");
    store.session.save(&parent).await.unwrap();

    let mgr = Arc::new(SessionManager::new(
        store.session.clone(),
        store.session_summary.clone(),
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
        parent_session_id: parent.id.clone(),
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

    reap_maintenance_orphans(&mgr, &paths).await;

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
    ));
    // Point at a workspace whose state/sessions dir was never created.
    let paths = WorkspacePaths::new(dir.path().join("empty-workspace"));
    // Must not panic / error.
    reap_maintenance_orphans(&mgr, &paths).await;
    assert!(!paths.state_sessions_dir().exists());
}

/// Cascade-on-delete: removing a parent session takes its
/// `session_summaries` row with it (`ON DELETE CASCADE` FK).
#[tokio::test]
async fn parent_delete_cascades_summary_metadata() {
    let (store, _dir) = fresh_store_and_paths().await;
    let parent = root_session("parent-cascade");
    store.session.save(&parent).await.unwrap();

    let mgr = SessionManager::new(store.session.clone(), store.session_summary.clone());

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

    let mgr = SessionManager::new(store.session.clone(), store.session_summary.clone());

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
