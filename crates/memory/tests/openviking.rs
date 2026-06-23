//! End-to-end tests for `baybo_memory::backends::openviking` against an axum mock server.

mod common;

use std::sync::Arc;

use axum::extract::{Path, RawQuery, State};
use axum::http::HeaderMap;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use baybo_memory::Memory;
use baybo_memory::backends::openviking::{
    OpenVikingConfig, OpenVikingMemory, TOOL_ARCHIVE_EXPAND, TOOL_FORGET, TOOL_RECALL, TOOL_STORE,
};
use baybo_model::{ChatMessage, ContentBlock};
use baybo_trace::StepKind;
use parking_lot::Mutex;
use serde_json::{Value, json};

use common::{base_url, memory_context, spawn, tool_context};

#[derive(Default, Clone)]
struct Captured {
    headers: Arc<Mutex<Vec<HeaderMap>>>,
    bodies: Arc<Mutex<Vec<Value>>>,
    paths: Arc<Mutex<Vec<String>>>,
    queries: Arc<Mutex<Vec<String>>>,
}

fn cfg(server_url: &str) -> OpenVikingConfig {
    OpenVikingConfig {
        endpoint: Some(server_url.to_string()),
        api_key_name: None,
        account: Some("acct".into()),
        top_k: Some(5),
    }
}

fn build(server_url: &str) -> OpenVikingMemory {
    OpenVikingMemory::new(cfg(server_url), "test-key".into(), None).unwrap()
}

fn tool(m: &OpenVikingMemory, name: &str) -> Arc<dyn baybo_tools::Tool> {
    m.tools()
        .into_iter()
        .find(|(t, _)| t.name() == name)
        .map(|(t, _)| t)
        .unwrap_or_else(|| panic!("tool {name} not found"))
}

fn expect_json(out: baybo_tools::ToolOutput) -> Value {
    match out {
        baybo_tools::ToolOutput::Json(v) => v,
        other => panic!("expected Json, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// recall (hook — unchanged: single /search/find with {query, top_k})
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recall_sends_query_and_returns_abstract_with_uri() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/api/v1/search/find",
            post(
                |State(c): State<Captured>, headers: HeaderMap, Json(body): Json<Value>| async move {
                    c.headers.lock().push(headers);
                    c.bodies.lock().push(body);
                    Json(json!({
                        "result": {
                            "memories": [
                                {"uri": "viking://m/1", "abstract": "user prefers Rust", "score": 0.9}
                            ],
                            "resources": []
                        }
                    }))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let ctx = memory_context("alice", "s-1", StepKind::MemoryRecall).await;
    let out = m
        .recall(&ctx, &[ContentBlock::Text("rust".into())])
        .await
        .unwrap();

    assert_eq!(out.len(), 1);
    assert!(out[0].content.contains("user prefers Rust"));
    assert!(out[0].content.contains("viking://m/1"));

    let headers = captured.headers.lock().last().cloned().unwrap();
    assert_eq!(headers.get("x-openviking-account").unwrap(), "acct");
    assert_eq!(headers.get("x-openviking-user").unwrap(), "alice");
    assert_eq!(headers.get("x-openviking-agent").unwrap(), "baybo");
    let body = captured.bodies.lock().last().unwrap().clone();
    assert_eq!(body["query"], "rust");
    assert_eq!(body["top_k"], 5);
}

#[tokio::test]
async fn recall_returns_empty_when_query_empty() {
    let app = Router::new();
    let server = spawn(app).await;
    let m = build(&base_url(&server));
    let ctx = memory_context("alice", "s-1", StepKind::MemoryRecall).await;
    assert!(m.recall(&ctx, &[]).await.unwrap().is_empty());
}

#[tokio::test]
async fn on_session_end_tolerates_slow_commit() {
    // Commit can be slow when the server runs LLM-backed extraction.
    // Verify the call waits well past `HTTP_TIMEOUT` (30s) — a 35-s
    // server-side stall completes successfully because WRITE_TIMEOUT
    // (10 min) is the actual budget on this code path.
    let app = Router::new().route(
        "/api/v1/sessions/{sid}/commit",
        post(|Path(_sid): Path<String>| async move {
            tokio::time::sleep(std::time::Duration::from_secs(35)).await;
            Json(json!({}))
        }),
    );
    let server = spawn(app).await;
    let m = build(&base_url(&server));
    let ctx = memory_context("alice", "slow-commit", StepKind::MemoryWrite).await;
    let transcript = vec![ChatMessage::user(vec![ContentBlock::Text("hi".into())])];
    let started = std::time::Instant::now();
    m.on_session_end(&ctx, &transcript).await.unwrap();
    let elapsed = started.elapsed();
    // The 35-s wait must have completed (not been cancelled at 30 s).
    assert!(
        elapsed >= std::time::Duration::from_secs(34),
        "on_session_end must wait through the slow commit; took {elapsed:?}"
    );
}

#[tokio::test]
async fn recall_returns_empty_on_critical_path_timeout() {
    // Handler stalls past the 5 s RECALL_TIMEOUT — recall must return empty
    // (no recalled context) instead of blocking the agent loop.
    let app = Router::new().route(
        "/api/v1/search/find",
        post(|Json(_b): Json<Value>| async move {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            Json(json!({"result": {"memories": [], "resources": []}}))
        }),
    );
    let server = spawn(app).await;
    let m = build(&base_url(&server));
    let ctx = memory_context("alice", "s-1", StepKind::MemoryRecall).await;
    let started = std::time::Instant::now();
    let out = m
        .recall(&ctx, &[ContentBlock::Text("hi".into())])
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert!(out.is_empty());
    assert!(
        elapsed < std::time::Duration::from_secs(8),
        "recall should not stall past RECALL_TIMEOUT; took {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// on_job_complete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn on_job_complete_posts_two_messages_keyed_on_session() {
    let captured = Captured::default();
    let app =
        Router::new()
            .route(
                "/api/v1/sessions/{sid}/messages",
                post(
                    |State(c): State<Captured>,
                     Path(sid): Path<String>,
                     Json(body): Json<Value>| async move {
                        c.paths.lock().push(sid);
                        c.bodies.lock().push(body);
                        Json(json!({}))
                    },
                ),
            )
            .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let ctx = memory_context("alice", "session-42", StepKind::MemoryWrite).await;
    m.on_job_complete(
        &ctx,
        &[ContentBlock::Text("hi".into())],
        &[ContentBlock::Text("hello".into())],
    )
    .await
    .unwrap();

    let paths = captured.paths.lock().clone();
    assert_eq!(paths.len(), 2);
    assert_eq!(paths[0], "session-42");
    assert_eq!(paths[1], "session-42");
    let bodies = captured.bodies.lock().clone();
    assert_eq!(bodies[0]["role"], "user");
    assert_eq!(bodies[0]["content"], "hi");
    assert_eq!(bodies[1]["role"], "assistant");
    assert_eq!(bodies[1]["content"], "hello");
}

// ---------------------------------------------------------------------------
// on_session_end
// ---------------------------------------------------------------------------

#[tokio::test]
async fn on_session_end_commits_when_transcript_non_empty() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/api/v1/sessions/{sid}/commit",
            post(
                |State(c): State<Captured>, Path(sid): Path<String>| async move {
                    c.paths.lock().push(sid);
                    Json(json!({}))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let ctx = memory_context("alice", "session-99", StepKind::MemoryWrite).await;
    let transcript = vec![ChatMessage::user(vec![ContentBlock::Text("hi".into())])];
    m.on_session_end(&ctx, &transcript).await.unwrap();
    let paths = captured.paths.lock().clone();
    assert_eq!(paths, vec!["session-99".to_string()]);
}

#[tokio::test]
async fn on_session_end_skips_commit_for_empty_transcript() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/api/v1/sessions/{sid}/commit",
            post(
                |State(c): State<Captured>, Path(sid): Path<String>| async move {
                    c.paths.lock().push(sid);
                    Json(json!({}))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let ctx = memory_context("alice", "session-empty", StepKind::MemoryWrite).await;
    m.on_session_end(&ctx, &[]).await.unwrap();
    assert!(captured.paths.lock().is_empty());
}

// ---------------------------------------------------------------------------
// tool: viking_recall
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_recall_queries_both_roots_merges_and_leaf_filters() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/api/v1/search/find",
            post(
                |State(c): State<Captured>, Json(body): Json<Value>| async move {
                    c.bodies.lock().push(body);
                    // Each leg returns one leaf + one non-leaf, same URIs across
                    // legs — exercises merge, dedup, and leaf-only filtering.
                    Json(json!({"result": {"memories": [
                        {"uri": "viking://user/memories/leaf.md", "abstract": "leaf fact", "score": 0.9, "level": 2},
                        {"uri": "viking://user/memories/dir", "abstract": "a dir", "score": 0.95, "level": 1}
                    ]}}))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let out = tool(&m, TOOL_RECALL)
        .execute(json!({"query": "rust"}), &tool_context("alice"))
        .await
        .unwrap();
    let v = expect_json(out);
    assert_eq!(v["count"], 1, "non-leaf dropped, duplicate leaf deduped");
    assert_eq!(v["results"][0]["uri"], "viking://user/memories/leaf.md");

    let bodies = captured.bodies.lock().clone();
    assert_eq!(bodies.len(), 2, "queries user + agent roots");
    let targets: Vec<&str> = bodies
        .iter()
        .map(|b| b["target_uri"].as_str().unwrap())
        .collect();
    assert!(targets.contains(&"viking://user/memories"));
    assert!(targets.contains(&"viking://agent/memories"));
}

#[tokio::test]
async fn tool_recall_single_target_uri_hits_one_leg() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/api/v1/search/find",
            post(
                |State(c): State<Captured>, Json(body): Json<Value>| async move {
                    c.bodies.lock().push(body);
                    Json(json!({"result": {"memories": []}}))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let _ = tool(&m, TOOL_RECALL)
        .execute(
            json!({"query": "x", "targetUri": "viking://user/memories/notes"}),
            &tool_context("alice"),
        )
        .await
        .unwrap();
    let bodies = captured.bodies.lock().clone();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["target_uri"], "viking://user/memories/notes");
}

#[tokio::test]
async fn tool_recall_rejects_missing_query() {
    let server = spawn(Router::new()).await;
    let m = build(&base_url(&server));
    let err = tool(&m, TOOL_RECALL)
        .execute(json!({}), &tool_context("alice"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("query"), "got: {err}");
}

// ---------------------------------------------------------------------------
// tool: viking_store
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_store_writes_message_then_commits() {
    let captured = Captured::default();
    let app =
        Router::new()
            .route(
                "/api/v1/sessions/{sid}/messages",
                post(
                    |State(c): State<Captured>,
                     Path(sid): Path<String>,
                     Json(body): Json<Value>| async move {
                        c.paths.lock().push(sid);
                        c.bodies.lock().push(body);
                        Json(json!({}))
                    },
                ),
            )
            .route(
                "/api/v1/sessions/{sid}/commit",
                // Synchronous completion (no task_id) → no polling.
                post(|Path(_sid): Path<String>| async {
                    Json(json!({"status": "completed", "memories_extracted": {"preferences": 2}}))
                }),
            )
            .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let out = tool(&m, TOOL_STORE)
        .execute(
            json!({"text": "user prefers TypeScript", "sessionId": "sess-x"}),
            &tool_context("alice"),
        )
        .await
        .unwrap();
    let v = expect_json(out);
    assert_eq!(v["session_id"], "sess-x");
    assert_eq!(v["status"], "completed");
    assert_eq!(v["memories_extracted"], 2);

    assert_eq!(captured.paths.lock().clone(), vec!["sess-x".to_string()]);
    let msg = captured.bodies.lock().last().unwrap().clone();
    assert_eq!(msg["role"], "user");
    assert_eq!(msg["content"], "user prefers TypeScript");
}

#[tokio::test]
async fn tool_store_polls_extraction_task_to_completion() {
    let app = Router::new()
        .route(
            "/api/v1/sessions/{sid}/messages",
            post(|Path(_sid): Path<String>, Json(_b): Json<Value>| async { Json(json!({})) }),
        )
        .route(
            "/api/v1/sessions/{sid}/commit",
            // Phase 1 returns a task id; Phase 2 finishes in the background.
            post(|Path(_sid): Path<String>| async {
                Json(json!({"status": "running", "task_id": "task-7"}))
            }),
        )
        .route(
            "/api/v1/tasks/task-7",
            get(|| async {
                Json(
                    json!({"status": "completed", "result": {"memories_extracted": {"events": 3}}}),
                )
            }),
        );
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let out = tool(&m, TOOL_STORE)
        .execute(
            json!({"text": "remember this", "sessionId": "sess-y"}),
            &tool_context("alice"),
        )
        .await
        .unwrap();
    let v = expect_json(out);
    assert_eq!(v["status"], "completed");
    assert_eq!(v["memories_extracted"], 3);
}

#[tokio::test]
async fn tool_store_rejects_empty_text() {
    let server = spawn(Router::new()).await;
    let m = build(&base_url(&server));
    let err = tool(&m, TOOL_STORE)
        .execute(json!({"text": ""}), &tool_context("alice"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("text"), "got: {err}");
}

// ---------------------------------------------------------------------------
// tool: viking_forget
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_forget_deletes_memory_uri() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/api/v1/fs",
            delete(
                |State(c): State<Captured>, RawQuery(q): RawQuery| async move {
                    c.queries.lock().push(q.unwrap_or_default());
                    Json(json!({}))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let out = tool(&m, TOOL_FORGET)
        .execute(
            json!({"uri": "viking://user/memories/preferences/mem_x.md"}),
            &tool_context("alice"),
        )
        .await
        .unwrap();
    let v = expect_json(out);
    assert_eq!(v["action"], "deleted");

    let q = captured.queries.lock().last().cloned().unwrap();
    assert!(q.contains("recursive=false"), "query was: {q}");
    assert!(q.contains("memories"), "query was: {q}");
}

#[tokio::test]
async fn tool_forget_refuses_non_memory_uri() {
    // No server: the safety gate must short-circuit before any HTTP.
    let server = spawn(Router::new()).await;
    let m = build(&base_url(&server));
    let out = tool(&m, TOOL_FORGET)
        .execute(
            json!({"uri": "viking://user/resources/doc.md"}),
            &tool_context("alice"),
        )
        .await
        .unwrap();
    match out {
        baybo_tools::ToolOutput::Error(e) => assert!(e.contains("non-memory"), "got: {e}"),
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn tool_forget_by_query_auto_deletes_strong_single_match() {
    let app = Router::new()
        .route(
            "/api/v1/search/find",
            post(|Json(_b): Json<Value>| async {
                Json(json!({"result": {"memories": [
                    {"uri": "viking://user/memories/only.md", "abstract": "the one", "score": 0.95, "level": 2}
                ]}}))
            }),
        )
        .route("/api/v1/fs", delete(|RawQuery(_q): RawQuery| async { Json(json!({})) }));
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let out = tool(&m, TOOL_FORGET)
        .execute(json!({"query": "the one"}), &tool_context("alice"))
        .await
        .unwrap();
    let v = expect_json(out);
    assert_eq!(v["action"], "deleted");
    assert_eq!(v["uri"], "viking://user/memories/only.md");
}

#[tokio::test]
async fn tool_forget_rejects_missing_params() {
    let server = spawn(Router::new()).await;
    let m = build(&base_url(&server));
    let err = tool(&m, TOOL_FORGET)
        .execute(json!({}), &tool_context("alice"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("uri or query"), "got: {err}");
}

// ---------------------------------------------------------------------------
// tool: viking_archive_expand
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_archive_expand_returns_original_messages() {
    // tool_context() sets session_id = "test-session".
    let app = Router::new().route(
        "/api/v1/sessions/{sid}/archives/{aid}",
        get(|Path((sid, aid)): Path<(String, String)>| async move {
            assert_eq!(sid, "test-session");
            Json(json!({"result": {
                "archive_id": aid,
                "abstract": "did some setup",
                "messages": [
                    {"role": "user", "content": "run the migration"},
                    {"role": "assistant", "content": "migration done"}
                ]
            }}))
        }),
    );
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let out = tool(&m, TOOL_ARCHIVE_EXPAND)
        .execute(json!({"archiveId": "archive_002"}), &tool_context("alice"))
        .await
        .unwrap();
    let v = expect_json(out);
    assert_eq!(v["action"], "expanded");
    assert_eq!(v["archive_id"], "archive_002");
    assert_eq!(v["message_count"], 2);
    let content = v["content"].as_str().unwrap();
    assert!(content.contains("run the migration"), "got: {content}");
    assert!(content.contains("migration done"), "got: {content}");
}

#[tokio::test]
async fn tool_archive_expand_rejects_missing_id() {
    let server = spawn(Router::new()).await;
    let m = build(&base_url(&server));
    let err = tool(&m, TOOL_ARCHIVE_EXPAND)
        .execute(json!({"archiveId": "  "}), &tool_context("alice"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("archiveId"), "got: {err}");
}

// ---------------------------------------------------------------------------
// tool manifest sanity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tools_each_carry_a_matching_manifest() {
    let server = spawn(Router::new()).await;
    let m = build(&base_url(&server));
    let tools = m.tools();
    let names: Vec<String> = tools.iter().map(|(t, _)| t.name().to_string()).collect();
    for required in [TOOL_RECALL, TOOL_STORE, TOOL_FORGET, TOOL_ARCHIVE_EXPAND] {
        assert!(names.contains(&required.to_string()), "missing: {required}");
    }
    for (tool, manifest) in &tools {
        assert_eq!(tool.name(), manifest.name);
        assert!(!manifest.description.is_empty());
        assert!(
            manifest
                .capabilities
                .contains(&baybo_tools::ToolCapability::Http),
            "{} should declare Http",
            tool.name()
        );
    }
}
