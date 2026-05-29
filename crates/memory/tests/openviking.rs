//! End-to-end tests for `aura_memory::openviking` against an axum mock server.

mod common;

use std::sync::Arc;

use aura_memory::Memory;
use aura_memory::openviking::{
    OpenVikingConfig, OpenVikingMemory, TOOL_ADD_RESOURCE, TOOL_BROWSE, TOOL_READ, TOOL_REMEMBER,
    TOOL_SEARCH,
};
use aura_model::{ChatMessage, ContentBlock};
use aura_trace::StepKind;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};

use common::{base_url, memory_context, spawn, tool_context};

#[derive(Default, Clone)]
struct Captured {
    headers: Arc<Mutex<Vec<HeaderMap>>>,
    bodies: Arc<Mutex<Vec<Value>>>,
    paths: Arc<Mutex<Vec<String>>>,
    queries: Arc<Mutex<Vec<String>>>,
}

#[derive(Deserialize)]
struct UriQuery {
    uri: String,
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

// ---------------------------------------------------------------------------
// recall
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
    assert_eq!(headers.get("x-openviking-agent").unwrap(), "aura");
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
// tool: viking_search
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_search_passes_mode_and_scope() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/api/v1/search/find",
            post(
                |State(c): State<Captured>, Json(body): Json<Value>| async move {
                    c.bodies.lock().push(body);
                    Json(json!({"result": {"memories": [], "resources": []}}))
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
    let ctx = tool_context("alice");
    let _ = tool
        .execute(
            json!({"query": "x", "mode": "deep", "scope": "viking://docs/", "limit": 7}),
            &ctx,
        )
        .await
        .unwrap();
    let body = captured.bodies.lock().last().unwrap().clone();
    assert_eq!(body["query"], "x");
    assert_eq!(body["mode"], "deep");
    assert_eq!(body["target_uri"], "viking://docs/");
    assert_eq!(body["top_k"], 7);
}

// ---------------------------------------------------------------------------
// tool: viking_read
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_read_uses_abstract_endpoint_for_pseudo_dir_uri() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/api/v1/content/abstract",
            get(
                |State(c): State<Captured>, Query(q): Query<UriQuery>| async move {
                    c.queries.lock().push(q.uri);
                    Json(json!({"content": "short summary"}))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let (tool, _) = m
        .tools()
        .into_iter()
        .find(|(t, _)| t.name() == TOOL_READ)
        .unwrap();
    let ctx = tool_context("alice");
    let out = tool
        .execute(
            json!({"uri": "viking://user/alice/.abstract.md", "level": "abstract"}),
            &ctx,
        )
        .await
        .unwrap();
    let payload = match out {
        aura_tools::ToolOutput::Json(v) => v,
        _ => panic!("expected Json"),
    };
    assert_eq!(payload["resolved_uri"], "viking://user/alice");
    assert_eq!(payload["content"], "short summary");
    let queried = captured.queries.lock().last().cloned().unwrap();
    assert_eq!(queried, "viking://user/alice");
}

// ---------------------------------------------------------------------------
// tool: viking_browse
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_browse_lists_and_normalises_entries() {
    let app = Router::new().route(
        "/api/v1/fs/ls",
        get(|Query(_q): Query<UriQuery>| async {
            Json(json!({
                "result": {
                    "entries": [
                        {"uri": "viking://a", "name": "a", "isDir": true},
                        {"uri": "viking://b", "name": "b.md", "is_dir": false}
                    ]
                }
            }))
        }),
    );
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let (tool, _) = m
        .tools()
        .into_iter()
        .find(|(t, _)| t.name() == TOOL_BROWSE)
        .unwrap();
    let ctx = tool_context("alice");
    let out = tool
        .execute(json!({"action": "list", "path": "viking://"}), &ctx)
        .await
        .unwrap();
    let payload = match out {
        aura_tools::ToolOutput::Json(v) => v,
        _ => panic!("expected Json"),
    };
    let entries = payload["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["type"], "dir");
    assert_eq!(entries[1]["type"], "file");
}

// ---------------------------------------------------------------------------
// tool: viking_remember
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_remember_writes_under_user_subdir() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/api/v1/content/write",
            post(
                |State(c): State<Captured>, Json(body): Json<Value>| async move {
                    c.bodies.lock().push(body);
                    Json(json!({"result": {"written_bytes": 42}}))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let (tool, _) = m
        .tools()
        .into_iter()
        .find(|(t, _)| t.name() == TOOL_REMEMBER)
        .unwrap();
    let ctx = tool_context("alice");
    let _ = tool
        .execute(
            json!({"content": "user prefers TypeScript", "category": "preference"}),
            &ctx,
        )
        .await
        .unwrap();
    let body = captured.bodies.lock().last().unwrap().clone();
    let uri = body["uri"].as_str().unwrap();
    assert!(
        uri.starts_with("viking://user/alice/memories/preferences/mem_"),
        "got URI: {uri}"
    );
    assert!(uri.ends_with(".md"));
    assert_eq!(body["mode"], "create");
    assert_eq!(body["content"], "user prefers TypeScript");
}

#[tokio::test]
async fn tool_remember_defaults_unknown_category_to_preferences() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/api/v1/content/write",
            post(
                |State(c): State<Captured>, Json(body): Json<Value>| async move {
                    c.bodies.lock().push(body);
                    Json(json!({"result": {"written_bytes": 0}}))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let (tool, _) = m
        .tools()
        .into_iter()
        .find(|(t, _)| t.name() == TOOL_REMEMBER)
        .unwrap();
    let ctx = tool_context("alice");
    let _ = tool.execute(json!({"content": "x"}), &ctx).await.unwrap();
    let uri = captured.bodies.lock().last().unwrap()["uri"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(uri.contains("/preferences/"), "got URI: {uri}");
}

// ---------------------------------------------------------------------------
// tool: viking_add_resource
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_add_resource_passes_remote_url_as_path() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/api/v1/resources",
            post(
                |State(c): State<Captured>, Json(body): Json<Value>| async move {
                    c.bodies.lock().push(body);
                    Json(json!({"result": {"root_uri": "viking://resources/r-1"}}))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let (tool, _) = m
        .tools()
        .into_iter()
        .find(|(t, _)| t.name() == TOOL_ADD_RESOURCE)
        .unwrap();
    let ctx = tool_context("alice");
    let out = tool
        .execute(
            json!({"url": "https://example.com/doc.md", "reason": "Reference"}),
            &ctx,
        )
        .await
        .unwrap();
    match out {
        aura_tools::ToolOutput::Json(v) => {
            assert_eq!(v["root_uri"], "viking://resources/r-1");
        }
        other => panic!("expected Json, got {other:?}"),
    }
    let body = captured.bodies.lock().last().unwrap().clone();
    assert_eq!(body["path"], "https://example.com/doc.md");
    assert_eq!(body["reason"], "Reference");
}

#[tokio::test]
async fn tool_add_resource_refuses_sensitive_local_path() {
    let app = Router::new();
    let server = spawn(app).await;
    let m = build(&base_url(&server));
    let (tool, _) = m
        .tools()
        .into_iter()
        .find(|(t, _)| t.name() == TOOL_ADD_RESOURCE)
        .unwrap();
    let ctx = tool_context("alice");
    // Pick a path that `is_sensitive_path` matches regardless of host
    // layout — the function uses suffix / directory matching on canonical
    // names, so a home-dir-style `~/.ssh/id_rsa` triggers it.
    let err = tool
        .execute(json!({"url": "~/.ssh/id_rsa"}), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("sensitive"), "got: {err}");
}

#[tokio::test]
async fn tool_add_resource_refuses_loopback_remote_url() {
    let app = Router::new();
    let server = spawn(app).await;
    let m = build(&base_url(&server));
    let (tool, _) = m
        .tools()
        .into_iter()
        .find(|(t, _)| t.name() == TOOL_ADD_RESOURCE)
        .unwrap();
    let ctx = tool_context("alice");
    // Loopback literal IP must be blocked even via the OpenViking server
    // (the server would otherwise become an SSRF fetcher onto our box).
    let err = tool
        .execute(json!({"url": "http://127.0.0.1:9000/x"}), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("blocked"), "got: {err}");

    // Loopback hostname names also blocked.
    let err = tool
        .execute(json!({"url": "http://localhost/x"}), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("blocked"), "got: {err}");

    // RFC1918 literal IP blocked.
    let err = tool
        .execute(json!({"url": "http://10.0.0.5/x"}), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("blocked"), "got: {err}");
}

#[tokio::test]
async fn tool_add_resource_rejects_both_to_and_parent() {
    let app = Router::new();
    let server = spawn(app).await;
    let m = build(&base_url(&server));
    let (tool, _) = m
        .tools()
        .into_iter()
        .find(|(t, _)| t.name() == TOOL_ADD_RESOURCE)
        .unwrap();
    let ctx = tool_context("alice");
    let err = tool
        .execute(
            json!({"url": "https://x", "to": "viking://a", "parent": "viking://b"}),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Cannot specify both"));
}

#[tokio::test]
async fn tool_add_resource_uploads_local_file() {
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/api/v1/resources/temp_upload",
            post(
                |State(c): State<Captured>, _body: axum::body::Bytes| async move {
                    c.bodies.lock().push(json!({"_marker": "uploaded"}));
                    Json(json!({"result": {"temp_file_id": "tf-123"}}))
                },
            ),
        )
        .route(
            "/api/v1/resources",
            post(
                |State(c): State<Captured>, Json(body): Json<Value>| async move {
                    c.bodies.lock().push(body);
                    Json(json!({"result": {"root_uri": "viking://resources/r-2"}}))
                },
            ),
        )
        .with_state(captured.clone());
    let server = spawn(app).await;
    let m = build(&base_url(&server));

    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), b"hello world").unwrap();
    let path = tmp.path().to_string_lossy().to_string();

    let (tool, _) = m
        .tools()
        .into_iter()
        .find(|(t, _)| t.name() == TOOL_ADD_RESOURCE)
        .unwrap();
    let ctx = tool_context("alice");
    let _ = tool.execute(json!({"url": path}), &ctx).await.unwrap();
    let bodies = captured.bodies.lock().clone();
    assert_eq!(bodies[0]["_marker"], "uploaded");
    assert_eq!(bodies[1]["temp_file_id"], "tf-123");
    assert!(bodies[1]["source_name"].is_string());
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
    for required in [
        TOOL_SEARCH,
        TOOL_READ,
        TOOL_BROWSE,
        TOOL_REMEMBER,
        TOOL_ADD_RESOURCE,
    ] {
        assert!(names.contains(&required.to_string()), "missing: {required}");
    }
    for (tool, manifest) in &tools {
        assert_eq!(tool.name(), manifest.name);
        assert!(!manifest.description.is_empty());
        assert!(
            manifest
                .capabilities
                .contains(&aura_tools::ToolCapability::Http),
            "{} should declare Http",
            tool.name()
        );
        if tool.name() == TOOL_ADD_RESOURCE {
            assert!(
                manifest
                    .capabilities
                    .contains(&aura_tools::ToolCapability::ReadFile),
                "viking_add_resource must declare ReadFile (it reads local paths)"
            );
        }
    }
}
