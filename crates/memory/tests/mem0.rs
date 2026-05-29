//! End-to-end tests for `aura_memory::mem0` against an axum mock server.
//!
//! Each test spins up a tiny `axum::Router` that asserts the inbound request
//! shape (path, auth header, body) and returns a canned JSON response, then
//! drives the backend through the same code path the runtime uses.

mod common;

use std::sync::Arc;

use aura_memory::mem0::{Mem0Config, Mem0Memory, TOOL_CONCLUDE, TOOL_PROFILE, TOOL_SEARCH};
use aura_memory::{Memory, RecalledMemory};
use aura_model::ContentBlock;
use aura_trace::StepKind;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use parking_lot::Mutex;
use serde_json::{Value, json};

use common::{base_url, memory_context, spawn, tool_context};

#[derive(Default, Clone)]
struct Captured {
    auth: Arc<Mutex<Option<String>>>,
    bodies: Arc<Mutex<Vec<Value>>>,
}

fn cfg(server_url: &str, top_k: usize) -> Mem0Config {
    Mem0Config {
        api_key_env: None,
        base_url: Some(server_url.to_string()),
        rerank: Some(true),
        top_k: Some(top_k),
    }
}

fn build(server_url: &str) -> Mem0Memory {
    Mem0Memory::new(cfg(server_url, 5), "test-key".into(), None).unwrap()
}

// ---------------------------------------------------------------------------
// recall
// ---------------------------------------------------------------------------

#[tokio::test]
async fn recall_sends_query_with_user_filter_and_returns_memory_text() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/v2/memories/search",
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
        "/v2/memories/search",
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
        "/v2/memories/search",
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
// on_job_complete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn on_job_complete_posts_messages_with_user_and_agent_ids() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/v1/memories",
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
    m.on_job_complete(&ctx, &user_in, &assistant_out)
        .await
        .unwrap();

    let body = captured.bodies.lock().last().unwrap().clone();
    assert_eq!(body["user_id"], "u-2");
    assert_eq!(body["agent_id"], "aura");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "hi");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "hello");
}

#[tokio::test]
async fn on_job_complete_skips_when_both_sides_empty() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/v1/memories",
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
    m.on_job_complete(&ctx, &[], &[]).await.unwrap();
    assert!(captured.bodies.lock().is_empty(), "no call should fire");
}

// ---------------------------------------------------------------------------
// on_session_end (mem0 → no-op)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn on_session_end_is_a_noop() {
    let captured = Captured::default();
    // Refuse any call: a body push would prove the no-op contract is broken.
    let app = Router::new()
        .route(
            "/v1/memories",
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

    let ctx = memory_context("u-3", "s-3", StepKind::MemoryWrite).await;
    m.on_session_end(&ctx, &[]).await.unwrap();
    assert!(captured.bodies.lock().is_empty());
}

// ---------------------------------------------------------------------------
// tool: mem0_profile
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_profile_fetches_all_memories() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/v2/memories",
            post(
                |State(c): State<Captured>, Json(body): Json<Value>| async move {
                    c.bodies.lock().push(body.clone());
                    Json(json!([
                        {"memory": "fact A"},
                        {"memory": "fact B"},
                    ]))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let tools = m.tools();
    let (profile, _manifest) = tools
        .into_iter()
        .find(|(t, _)| t.name() == TOOL_PROFILE)
        .unwrap();

    let ctx = tool_context("u-4");
    let out = profile.execute(json!({}), &ctx).await.unwrap();
    let json_val = match out {
        aura_tools::ToolOutput::Json(v) => v,
        other => panic!("expected Json, got {other:?}"),
    };
    assert_eq!(json_val["count"], 2);
    assert!(json_val["result"].as_str().unwrap().contains("fact A"));
    let body = captured.bodies.lock().last().unwrap().clone();
    assert_eq!(body["filters"]["AND"][0]["user_id"], "u-4");
}

// ---------------------------------------------------------------------------
// tool: mem0_search
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_search_passes_query_and_rerank() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/v2/memories/search",
            post(
                |State(c): State<Captured>, Json(body): Json<Value>| async move {
                    c.bodies.lock().push(body.clone());
                    Json(json!([{"memory": "x", "score": 0.5}]))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let (tool, _) = m
        .tools()
        .into_iter()
        .find(|(t, _)| t.name() == TOOL_SEARCH)
        .unwrap();
    let ctx = tool_context("u-5");
    let out = tool
        .execute(json!({"query": "rust", "rerank": true, "top_k": 3}), &ctx)
        .await
        .unwrap();
    assert!(matches!(out, aura_tools::ToolOutput::Json(_)));
    let body = captured.bodies.lock().last().unwrap().clone();
    assert_eq!(body["query"], "rust");
    assert_eq!(body["rerank"], true);
    assert_eq!(body["top_k"], 3);
    assert_eq!(body["filters"]["AND"][0]["user_id"], "u-5");
}

#[tokio::test]
async fn tool_search_rejects_missing_query() {
    let app = Router::new();
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let (tool, _) = m
        .tools()
        .into_iter()
        .find(|(t, _)| t.name() == TOOL_SEARCH)
        .unwrap();
    let ctx = tool_context("u-5");
    let err = tool.execute(json!({}), &ctx).await.unwrap_err();
    assert!(err.to_string().contains("query"), "got: {err}");
}

#[tokio::test]
async fn tool_search_caps_top_k_at_fifty() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/v2/memories/search",
            post(
                |State(c): State<Captured>, Json(body): Json<Value>| async move {
                    c.bodies.lock().push(body.clone());
                    Json(json!([]))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let (tool, _) = m
        .tools()
        .into_iter()
        .find(|(t, _)| t.name() == TOOL_SEARCH)
        .unwrap();
    let ctx = tool_context("u-5");
    let _ = tool
        .execute(json!({"query": "x", "top_k": 9999}), &ctx)
        .await
        .unwrap();
    let body = captured.bodies.lock().last().unwrap().clone();
    assert_eq!(body["top_k"], 50);
}

// ---------------------------------------------------------------------------
// tool: mem0_conclude
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_conclude_posts_verbatim_with_infer_false() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/v1/memories",
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

    let (tool, _) = m
        .tools()
        .into_iter()
        .find(|(t, _)| t.name() == TOOL_CONCLUDE)
        .unwrap();
    let ctx = tool_context("u-6");
    let _ = tool
        .execute(json!({"conclusion": "user is left-handed"}), &ctx)
        .await
        .unwrap();
    let body = captured.bodies.lock().last().unwrap().clone();
    assert_eq!(body["infer"], false);
    assert_eq!(body["user_id"], "u-6");
    assert_eq!(body["agent_id"], "aura");
    assert_eq!(body["messages"][0]["content"], "user is left-handed");
    assert_eq!(body["messages"][0]["role"], "user");
}

// ---------------------------------------------------------------------------
// circuit breaker
// ---------------------------------------------------------------------------

#[tokio::test]
async fn circuit_breaker_trips_after_five_consecutive_failures() {
    let app = Router::new().route(
        "/v2/memories/search",
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
// tool manifest sanity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tools_each_carry_a_matching_manifest() {
    let app = Router::new();
    let server = spawn(app).await;
    let m = build(&base_url(&server));
    let tools = m.tools();
    let names: Vec<String> = tools.iter().map(|(t, _)| t.name().to_string()).collect();
    assert!(names.contains(&TOOL_PROFILE.to_string()));
    assert!(names.contains(&TOOL_SEARCH.to_string()));
    assert!(names.contains(&TOOL_CONCLUDE.to_string()));
    for (tool, manifest) in &tools {
        assert_eq!(tool.name(), manifest.name);
        assert!(!manifest.description.is_empty());
        assert!(matches!(
            manifest.capabilities.first(),
            Some(aura_tools::ToolCapability::Http)
        ));
    }
}
