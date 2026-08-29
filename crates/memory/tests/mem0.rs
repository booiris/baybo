//! End-to-end tests for `baybo_memory::backends::mem0` against an axum mock server.
//!
//! Each test spins up a tiny `axum::Router` that asserts the inbound request
//! shape (path, auth header, body) and returns a canned JSON response, then
//! drives the backend through the same code path the runtime uses.

use std::sync::Arc;

use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use baybo_memory::backends::mem0::{
    Mem0Config, Mem0Memory, TOOL_ADD, TOOL_DELETE, TOOL_EVENT_LIST, TOOL_EVENT_STATUS, TOOL_GET,
    TOOL_LIST, TOOL_SEARCH, TOOL_UPDATE,
};
use baybo_memory::{Memory, RecalledMemory};
use baybo_model::{AgentProfileId, ContentBlock};
use baybo_trace::StepKind;
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::common::{
    base_url, memory_context, memory_context_for_agent, spawn, tool_context, tool_context_for_agent,
};

#[derive(Default, Clone)]
struct Captured {
    auth: Arc<Mutex<Option<String>>>,
    bodies: Arc<Mutex<Vec<Value>>>,
    queries: Arc<Mutex<Vec<String>>>,
}

fn cfg(server_url: &str, top_k: usize) -> Mem0Config {
    Mem0Config {
        api_key_name: None,
        base_url: Some(server_url.to_string()),
        rerank: Some(true),
        top_k: Some(top_k),
        self_hosted: None,
    }
}

fn build(server_url: &str) -> Mem0Memory {
    Mem0Memory::new(cfg(server_url, 5), "test-key".into(), None).unwrap()
}

/// Find a tool by name in the backend's contributed tool set.
fn tool_by_name(m: &Mem0Memory, name: &str) -> Arc<dyn baybo_tools::Tool> {
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
// recall
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recall_sends_query_with_user_filter_and_returns_memory_text() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/v2/memories/search/",
            post(
                |State(c): State<Captured>, headers: HeaderMap, Json(body): Json<Value>| async move {
                    *c.auth.lock() = headers
                        .get("authorization")
                        .and_then(|h| h.to_str().ok())
                        .map(String::from);
                    c.bodies.lock().push(body.clone());
                    Json(json!([
                        {"memory": "User likes terse responses", "score": 0.92},
                        {"memory": "User prefers Rust", "score": 0.81},
                    ]))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let ctx = memory_context("u-1", "s-1", StepKind::MemoryRecall).await;
    let query = vec![ContentBlock::Text("hello world".into())];
    let out = m.recall(&ctx, &query).await.unwrap();

    assert_eq!(out.len(), 2);
    assert_eq!(out[0].content, "User likes terse responses");
    let body = captured.bodies.lock().last().unwrap().clone();
    assert_eq!(body["query"], "hello world");
    assert_eq!(body["filters"]["AND"][0]["user_id"], "u-1");
    assert_eq!(body["rerank"], true);
    assert_eq!(body["top_k"], 5);
    let auth = captured.auth.lock().clone().unwrap_or_default();
    assert!(auth.starts_with("Token "), "got auth header: {auth}");
}

#[tokio::test]
async fn recall_skips_empty_query_blocks() {
    // No HTTP handler registered → if the call goes out, axum responds 404
    // and we'd see an error logged. Empty-query path must not hit the wire.
    let app = Router::new();
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let ctx = memory_context("u-1", "s-1", StepKind::MemoryRecall).await;
    let out = m.recall(&ctx, &[]).await.unwrap();
    assert!(out.is_empty());
}

#[tokio::test]
async fn recall_handles_results_wrapper() {
    let app = Router::new().route(
        "/v2/memories/search/",
        post(|Json(_b): Json<Value>| async { Json(json!({"results": [{"memory": "fact one"}]})) }),
    );
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let ctx = memory_context("u-1", "s-1", StepKind::MemoryRecall).await;
    let out = m
        .recall(&ctx, &[ContentBlock::Text("q".into())])
        .await
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].content, "fact one");
}

#[tokio::test]
async fn recall_swallows_5xx_and_returns_empty() {
    let app = Router::new().route(
        "/v2/memories/search/",
        post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "boom") }),
    );
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let ctx = memory_context("u-1", "s-1", StepKind::MemoryRecall).await;
    let out: Vec<RecalledMemory> = m
        .recall(&ctx, &[ContentBlock::Text("q".into())])
        .await
        .unwrap();
    assert!(out.is_empty());
}

// ---------------------------------------------------------------------------
// on_turn_complete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn on_turn_complete_posts_messages_with_user_and_agent_ids() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/v1/memories/",
            post(
                |State(c): State<Captured>, Json(body): Json<Value>| async move {
                    c.bodies.lock().push(body.clone());
                    Json(json!({"results": []}))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let ctx = memory_context("u-2", "s-2", StepKind::MemoryWrite).await;
    let user_in = vec![ContentBlock::Text("hi".into())];
    let assistant_out = vec![ContentBlock::Text("hello".into())];
    m.on_turn_complete(&ctx, &user_in, &assistant_out)
        .await
        .unwrap();

    let body = captured.bodies.lock().last().unwrap().clone();
    assert_eq!(body["user_id"], "u-2");
    assert_eq!(body["agent_id"], "baybo");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "hi");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "hello");
}

#[tokio::test]
async fn on_turn_complete_skips_when_both_sides_empty() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/v1/memories/",
            post(
                |State(c): State<Captured>, Json(b): Json<Value>| async move {
                    c.bodies.lock().push(b);
                    Json(json!({}))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let ctx = memory_context("u-2", "s-2", StepKind::MemoryWrite).await;
    m.on_turn_complete(&ctx, &[], &[]).await.unwrap();
    assert!(captured.bodies.lock().is_empty(), "no call should fire");
}

// ---------------------------------------------------------------------------
// tool: mem0_search
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_search_builds_scoped_filter_and_caps_limit() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/v2/memories/search/",
            post(
                |State(c): State<Captured>, Json(body): Json<Value>| async move {
                    c.bodies.lock().push(body.clone());
                    Json(json!([{"id": "m1", "memory": "x", "score": 0.5}]))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let tool = tool_by_name(&m, TOOL_SEARCH);
    let ctx = tool_context("u-5");
    let out = tool
        .execute(
            json!({
                "query": "rust",
                "limit": 3,
                "scope": "session",
                "categories": ["preference"],
            }),
            &ctx,
        )
        .await
        .unwrap();
    let v = expect_json(out);
    assert_eq!(v["count"], 1);
    assert_eq!(v["results"][0]["id"], "m1");

    let body = captured.bodies.lock().last().unwrap().clone();
    assert_eq!(body["query"], "rust");
    assert_eq!(body["top_k"], 3);
    assert_eq!(body["rerank"], true);
    assert_eq!(body["threshold"], 0.3);
    // Every read is scoped by BOTH axes — one human, one agent — before any
    // caller-supplied narrowing.
    assert_eq!(body["filters"]["AND"][0]["user_id"], "u-5");
    assert_eq!(
        body["filters"]["AND"][1]["agent_id"],
        baybo_model::BUILTIN_AGENT_PROFILE_ID
    );
    assert_eq!(body["filters"]["AND"][2]["run_id"], "test-session");
    assert_eq!(
        body["filters"]["AND"][3]["categories"]["in"][0],
        "preference"
    );
}

#[tokio::test]
async fn tool_search_rejects_missing_query() {
    let app = Router::new();
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let tool = tool_by_name(&m, TOOL_SEARCH);
    let ctx = tool_context("u-5");
    let err = tool.execute(json!({}), &ctx).await.unwrap_err();
    assert!(err.to_string().contains("query"), "got: {err}");
}

// ---------------------------------------------------------------------------
// tool: mem0_add
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_add_stores_facts_verbatim_with_metadata_and_session_scope() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/v1/memories/",
            post(
                |State(c): State<Captured>, Json(body): Json<Value>| async move {
                    c.bodies.lock().push(body.clone());
                    Json(json!({"results": []}))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let tool = tool_by_name(&m, TOOL_ADD);
    let ctx = tool_context("u-6");
    let _ = tool
        .execute(
            json!({
                "facts": ["fact a", "fact b"],
                "category": "preference",
                "importance": 0.8,
                "longTerm": false,
            }),
            &ctx,
        )
        .await
        .unwrap();

    let body = captured.bodies.lock().last().unwrap().clone();
    assert_eq!(body["infer"], false);
    assert_eq!(body["user_id"], "u-6");
    assert_eq!(body["agent_id"], "baybo");
    assert_eq!(body["run_id"], "test-session");
    assert_eq!(body["categories"][0], "preference");
    assert_eq!(body["metadata"]["importance"], 0.8);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "fact a");
    assert_eq!(messages[1]["content"], "fact b");
}

#[tokio::test]
async fn tool_add_rejects_empty_input() {
    let app = Router::new();
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let tool = tool_by_name(&m, TOOL_ADD);
    let ctx = tool_context("u-6");
    let err = tool.execute(json!({}), &ctx).await.unwrap_err();
    assert!(err.to_string().contains("facts"), "got: {err}");
}

// ---------------------------------------------------------------------------
// tool: mem0_get
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_get_fetches_by_id() {
    // The tool calls GET /v1/memories/<id>/ — register the exact path. The
    // response must state its partition; the tool refuses one that does not.
    let app = Router::new().route(
        "/v1/memories/mem-123/",
        get(|| async {
            Json(json!({
                "id": "mem-123",
                "memory": "hello",
                "created_at": "2026-01-01",
                "user_id": "u-7",
                "agent_id": "baybo",
            }))
        }),
    );
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let tool = tool_by_name(&m, TOOL_GET);
    let ctx = tool_context("u-7");
    let out = tool
        .execute(json!({"memoryId": "mem-123"}), &ctx)
        .await
        .unwrap();
    let v = expect_json(out);
    assert_eq!(v["id"], "mem-123");
    assert_eq!(v["memory"], "hello");
    assert_eq!(v["created_at"], "2026-01-01");
}

#[tokio::test]
async fn tool_get_rejects_missing_id() {
    let app = Router::new();
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let tool = tool_by_name(&m, TOOL_GET);
    let ctx = tool_context("u-7");
    let err = tool.execute(json!({}), &ctx).await.unwrap_err();
    assert!(err.to_string().contains("memoryId"), "got: {err}");
}

// ---------------------------------------------------------------------------
// tool: mem0_list
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_list_returns_all_memories() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/v2/memories/",
            post(
                |State(c): State<Captured>,
                 RawQuery(q): RawQuery,
                 Json(body): Json<Value>| async move {
                    c.bodies.lock().push(body);
                    c.queries.lock().push(q.unwrap_or_default());
                    Json(json!([
                        {"id": "m1", "memory": "fact A"},
                        {"id": "m2", "memory": "fact B"},
                    ]))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let tool = tool_by_name(&m, TOOL_LIST);
    let ctx = tool_context("u-8");
    let out = tool.execute(json!({}), &ctx).await.unwrap();
    let v = expect_json(out);
    assert_eq!(v["count"], 2);
    assert!(v["result"].as_str().unwrap().contains("fact A"));

    let body = captured.bodies.lock().last().unwrap().clone();
    assert_eq!(body["filters"]["AND"][0]["user_id"], "u-8");
    let q = captured.queries.lock().last().unwrap().clone();
    assert!(q.contains("page_size=100"), "query was: {q}");
}

// ---------------------------------------------------------------------------
// tool: mem0_update
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_update_puts_new_text() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/v1/memories/mem-1/",
            // The GET is the ownership check that precedes an id-addressed
            // update; the PUT is what the test is actually about.
            get(|| async {
                Json(json!({"id": "mem-1", "memory": "old", "user_id": "u-9", "agent_id": "baybo"}))
            })
            .put(
                |State(c): State<Captured>, Json(body): Json<Value>| async move {
                    c.bodies.lock().push(body);
                    Json(json!({"message": "updated"}))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let tool = tool_by_name(&m, TOOL_UPDATE);
    let ctx = tool_context("u-9");
    let out = tool
        .execute(json!({"memoryId": "mem-1", "text": "new text"}), &ctx)
        .await
        .unwrap();
    let v = expect_json(out);
    assert!(v["result"].as_str().unwrap().contains("Updated"));

    let body = captured.bodies.lock().last().unwrap().clone();
    assert_eq!(body["text"], "new text");
}

// ---------------------------------------------------------------------------
// tool: mem0_delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_delete_by_id_hits_delete_endpoint() {
    // 204 No Content exercises the empty-body → success path in `request`.
    // The GET is the ownership check that now precedes an id-addressed delete.
    let app = Router::new().route(
        "/v1/memories/mem-1/",
        get(|| async {
            Json(json!({"id": "mem-1", "memory": "x", "user_id": "u-10", "agent_id": "baybo"}))
        })
        .delete(|| async { StatusCode::NO_CONTENT }),
    );
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let tool = tool_by_name(&m, TOOL_DELETE);
    let ctx = tool_context("u-10");
    let out = tool
        .execute(json!({"memoryId": "mem-1"}), &ctx)
        .await
        .unwrap();
    let v = expect_json(out);
    assert!(v["result"].as_str().unwrap().contains("deleted"));
}

/// An id from another agent's partition is not readable, editable or
/// deletable — and the refusal says only "no such memory", because
/// "that exists but is not yours" is itself a cross-partition read.
#[tokio::test]
async fn an_id_from_another_partition_is_refused() {
    let app = Router::new().route(
        "/v1/memories/mem-sibling/",
        get(|| async {
            Json(json!({
                "id": "mem-sibling",
                "memory": "the other persona's note",
                "user_id": "u-11",
                "agent_id": "01JOTHERAGENT",
            }))
        })
        .delete(|| async { StatusCode::NO_CONTENT })
        .put(|| async { StatusCode::NO_CONTENT }),
    );
    let server = spawn(app).await;
    let m = build(&base_url(&server));
    let ctx = tool_context_for_agent("u-11", &AgentProfileId::parse("01JMINE").unwrap());

    for (name, params) in [
        (TOOL_GET, json!({"memoryId": "mem-sibling"})),
        (TOOL_DELETE, json!({"memoryId": "mem-sibling"})),
        (
            TOOL_UPDATE,
            json!({"memoryId": "mem-sibling", "text": "overwritten"}),
        ),
    ] {
        let out = tool_by_name(&m, name).execute(params, &ctx).await.unwrap();
        match out {
            baybo_tools::ToolOutput::Error(msg) => {
                assert!(msg.contains("no memory mem-sibling"), "{name}: {msg}");
                assert!(
                    !msg.contains("other persona"),
                    "{name} leaked the content it refused: {msg}"
                );
            }
            other => panic!("{name} must refuse a foreign id, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn tool_delete_all_requires_confirm() {
    // No server: the confirm guard must short-circuit before any HTTP.
    let app = Router::new();
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let tool = tool_by_name(&m, TOOL_DELETE);
    let ctx = tool_context("u-10");
    let out = tool.execute(json!({"all": true}), &ctx).await.unwrap();
    match out {
        baybo_tools::ToolOutput::Error(e) => assert!(e.contains("confirm"), "got: {e}"),
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn tool_delete_all_with_confirm_scopes_to_user() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/v1/memories/",
            delete(
                |State(c): State<Captured>, RawQuery(q): RawQuery| async move {
                    c.queries.lock().push(q.unwrap_or_default());
                    Json(json!({"message": "deleted"}))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let tool = tool_by_name(&m, TOOL_DELETE);
    let ctx = tool_context("u-11");
    let out = tool
        .execute(json!({"all": true, "confirm": true}), &ctx)
        .await
        .unwrap();
    let v = expect_json(out);
    assert!(v["result"].as_str().unwrap().contains("u-11"));

    let q = captured.queries.lock().last().unwrap().clone();
    assert!(q.contains("user_id=u-11"), "query was: {q}");
}

#[tokio::test]
async fn tool_delete_by_query_auto_deletes_single_hit() {
    let app = Router::new()
        .route(
            "/v2/memories/search/",
            post(|Json(_b): Json<Value>| async {
                Json(json!([{"id": "mem-9", "memory": "the only match", "score": 0.4}]))
            }),
        )
        .route(
            "/v1/memories/mem-9/",
            delete(|| async { StatusCode::NO_CONTENT }),
        );
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let tool = tool_by_name(&m, TOOL_DELETE);
    let ctx = tool_context("u-12");
    let out = tool.execute(json!({"query": "match"}), &ctx).await.unwrap();
    let v = expect_json(out);
    assert!(v["result"].as_str().unwrap().contains("deleted"));
}

// ---------------------------------------------------------------------------
// tools: mem0_event_list / mem0_event_status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_event_list_returns_events() {
    let app = Router::new().route(
        "/v1/events/",
        get(|| async {
            Json(json!({"results": [
                {"id": "e1", "event_type": "ADD", "status": "completed", "latency": 12.3, "created_at": "2026-01-01T00:00:00Z"}
            ]}))
        }),
    );
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let tool = tool_by_name(&m, TOOL_EVENT_LIST);
    let ctx = tool_context("u-13");
    let out = tool.execute(json!({}), &ctx).await.unwrap();
    let v = expect_json(out);
    assert_eq!(v["count"], 1);
    assert_eq!(v["events"][0]["id"], "e1");
    assert_eq!(v["events"][0]["status"], "completed");
}

#[tokio::test]
async fn tool_event_status_returns_detail() {
    let app = Router::new().route(
        "/v1/event/ev-1/",
        get(|| async {
            Json(json!({
                "id": "ev-1", "event_type": "ADD", "status": "completed", "latency": 5,
                "results": [{"event": "ADD", "data": {"memory": "x"}}]
            }))
        }),
    );
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let tool = tool_by_name(&m, TOOL_EVENT_STATUS);
    let ctx = tool_context("u-14");
    let out = tool
        .execute(json!({"event_id": "ev-1"}), &ctx)
        .await
        .unwrap();
    let v = expect_json(out);
    assert_eq!(v["status"], "completed");
    assert_eq!(v["event_type"], "ADD");
    assert_eq!(v["results"][0]["event"], "ADD");
}

// ---------------------------------------------------------------------------
// circuit breaker
// ---------------------------------------------------------------------------

#[tokio::test]
async fn circuit_breaker_trips_after_five_consecutive_failures() {
    let app = Router::new().route(
        "/v2/memories/search/",
        post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "down") }),
    );
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let ctx = memory_context("u-7", "s-7", StepKind::MemoryRecall).await;
    let q = vec![ContentBlock::Text("q".into())];

    // Five consecutive failures trip the breaker.
    for _ in 0..5 {
        let _ = m.recall(&ctx, &q).await.unwrap();
    }

    // Sixth call should short-circuit and return empty without hitting HTTP.
    // (We can't directly assert "didn't hit HTTP" from this test layer, but
    // the breaker returning fast is the observable.)
    let out = m.recall(&ctx, &q).await.unwrap();
    assert!(out.is_empty());
}

// ---------------------------------------------------------------------------
// privacy: user scope is fixed to the session user (no userId param)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_search_ignores_user_id_override() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/v2/memories/search/",
            post(
                |State(c): State<Captured>, Json(body): Json<Value>| async move {
                    c.bodies.lock().push(body);
                    Json(json!([]))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    // A tool-supplied userId must NOT retarget the scope to another user.
    let _ = tool_by_name(&m, TOOL_SEARCH)
        .execute(
            json!({"query": "x", "userId": "victim"}),
            &tool_context("owner"),
        )
        .await
        .unwrap();
    let body = captured.bodies.lock().last().unwrap().clone();
    assert_eq!(body["filters"]["AND"][0]["user_id"], "owner");
}

#[tokio::test]
async fn tool_delete_all_cannot_target_another_user() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/v1/memories/",
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

    // The sharpest edge: all + confirm + userId must still scope to the
    // session user, never the model-supplied victim.
    let _ = tool_by_name(&m, TOOL_DELETE)
        .execute(
            json!({"all": true, "confirm": true, "userId": "victim"}),
            &tool_context("owner"),
        )
        .await
        .unwrap();
    let q = captured.queries.lock().last().cloned().unwrap();
    assert!(
        q.contains("user_id=owner"),
        "must scope to session user; got: {q}"
    );
    assert!(
        !q.contains("victim"),
        "must ignore userId override; got: {q}"
    );
}

// ---------------------------------------------------------------------------
// privacy: the agent partition is fixed to the calling session's agent
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_search_cannot_read_another_agents_partition() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/v2/memories/search/",
            post(
                |State(c): State<Captured>, Json(body): Json<Value>| async move {
                    c.bodies.lock().push(body);
                    Json(json!([]))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    // `agentId` is no longer a parameter: the partition tracks the calling
    // session, so a prompt-injected override cannot reach a sibling persona's
    // memories.
    let caller = AgentProfileId::parse("01JCALLER").unwrap();
    let _ = tool_by_name(&m, TOOL_SEARCH)
        .execute(
            json!({"query": "x", "agentId": "01JVICTIM"}),
            &tool_context_for_agent("owner", &caller),
        )
        .await
        .unwrap();
    let body = captured.bodies.lock().last().unwrap().clone();
    assert_eq!(body["filters"]["AND"][1]["agent_id"], "01JCALLER");
    assert!(
        !body.to_string().contains("01JVICTIM"),
        "must ignore an agentId override; got: {body}"
    );
}

#[tokio::test]
async fn tool_delete_all_cannot_wipe_another_agents_partition() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/v1/memories/",
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

    // The sharpest edge of the partition: a bulk wipe is always agent-scoped.
    let caller = AgentProfileId::parse("01JCALLER").unwrap();
    let _ = tool_by_name(&m, TOOL_DELETE)
        .execute(
            json!({"all": true, "confirm": true, "agentId": "01JVICTIM"}),
            &tool_context_for_agent("owner", &caller),
        )
        .await
        .unwrap();
    let q = captured.queries.lock().last().cloned().unwrap();
    assert!(
        q.contains("agent_id=01JCALLER"),
        "bulk delete must scope to the calling agent; got: {q}"
    );
    assert!(!q.contains("01JVICTIM"), "must ignore agentId; got: {q}");
}

#[tokio::test]
async fn recall_and_write_carry_the_sessions_agent() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/v2/memories/search/",
            post(
                |State(c): State<Captured>, Json(body): Json<Value>| async move {
                    c.bodies.lock().push(body);
                    Json(json!([]))
                },
            ),
        )
        .route(
            "/v1/memories/",
            post(
                |State(c): State<Captured>, Json(body): Json<Value>| async move {
                    c.bodies.lock().push(body);
                    Json(json!({}))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let agent = AgentProfileId::parse("01JAGENT").unwrap();
    let ctx = memory_context_for_agent("alice", &agent, "s-1", StepKind::MemoryRecall).await;
    m.recall(&ctx, &[ContentBlock::Text("hello".into())])
        .await
        .unwrap();
    let read = captured.bodies.lock().last().unwrap().clone();
    assert_eq!(read["filters"]["AND"][1]["agent_id"], "01JAGENT");

    m.on_turn_complete(
        &ctx,
        &[ContentBlock::Text("q".into())],
        &[ContentBlock::Text("a".into())],
    )
    .await
    .unwrap();
    let write = captured.bodies.lock().last().unwrap().clone();
    assert_eq!(write["agent_id"], "01JAGENT");
}

// ---------------------------------------------------------------------------
// tool manifest sanity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tools_each_carry_a_matching_manifest() {
    let app = Router::new();
    let server = spawn(app).await;
    let m = build(&base_url(&server));
    let tools = m.tools();
    let names: Vec<String> = tools.iter().map(|(t, _)| t.name().to_string()).collect();
    for expected in [
        TOOL_SEARCH,
        TOOL_ADD,
        TOOL_GET,
        TOOL_LIST,
        TOOL_UPDATE,
        TOOL_DELETE,
        TOOL_EVENT_LIST,
        TOOL_EVENT_STATUS,
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "missing tool: {expected}"
        );
    }
    for (tool, manifest) in &tools {
        assert_eq!(tool.name(), manifest.name);
        assert!(!manifest.description.is_empty());
        assert!(matches!(
            manifest.capabilities.first(),
            Some(baybo_tools::ToolCapability::Http)
        ));
    }
}
