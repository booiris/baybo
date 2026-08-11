//! `GET /v1/traces/tool-sets/{hash}` — the tool definitions an `LlmCall`
//! span's `tools.hash` names.
//!
//! The route sits under `/v1/traces/`, one segment shape away from
//! `/v1/traces/{session_id}/lineage`. Nothing in the type system stops a
//! request for `tool-sets/<hash>` from being routed to the lineage handler
//! with `session_id = "tool-sets"` — that would answer 200 with an empty
//! lineage instead of the tool set, which reads as "this call offered no
//! tools" rather than as a bug. So the happy path here is a routing test as
//! much as a payload test.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;

use baybo_gateway::test_support::{TEST_ADMIN_TOKEN, build_test_deps};
use baybo_model::ToolSetHash;
use baybo_store::TraceStore;
use baybo_trace::{LlmToolDefinition, LlmToolSet};

fn auth(req: Request<Body>) -> Request<Body> {
    let (mut parts, body) = req.into_parts();
    parts.headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {TEST_ADMIN_TOKEN}").parse().unwrap(),
    );
    Request::from_parts(parts, body)
}

/// A router plus the trace store behind it, so a test can seed the very row
/// the endpoint reads.
async fn build_router() -> (axum::Router, std::sync::Arc<dyn TraceStore>) {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let trace_store = tg.deps.stores.trace.clone();

    use baybo_gateway::auth::admin::{AdminAuthState, require_admin_token};
    let auth_state = AdminAuthState::new(tg.deps.admin_token.clone());
    let state = baybo_gateway::server::AdminState {
        workspace_paths: std::sync::Arc::clone(&tg.deps.workspace_paths),
        config: std::sync::Arc::clone(&tg.deps.config),
        config_path: tg.deps.config_path.clone(),
        session_manager: std::sync::Arc::clone(&tg.deps.session_manager),
        turn_lifecycle: std::sync::Arc::clone(&tg.deps.turn_lifecycle),
        cron_scheduler: std::sync::Arc::clone(&tg.deps.cron_scheduler),
        trace_store: tg.deps.stores.trace.clone(),
        cost_store: tg.deps.stores.cost.clone(),
        message_search: tg.deps.stores.message_search.clone(),
        query_api: std::sync::Arc::new(baybo_query::QueryApi::new(
            tg.deps.session_manager.store(),
            std::sync::Arc::clone(&tg.deps.turn_lifecycle),
            tg.deps.stores.trace.clone(),
            tg.deps.stores.cost.clone(),
        )),
        skill_registry: std::sync::Arc::clone(&tg.deps.skill_registry),
        tool_registry: std::sync::Arc::clone(&tg.deps.tool_registry),
        channel_registry: std::sync::Arc::clone(&tg.deps.channel_registry),
        llm_pool: tg.deps.llm_pool.clone(),
        supervisor: tg.deps.supervisor.clone(),
        config_reloader: tg.deps.config_reloader.clone(),
        log_buffer: std::sync::Arc::clone(&tg.deps.log_buffer),
        channel_bot_store: tg.deps.stores.channel_bot.clone(),
        agent_profile_store: tg.deps.stores.agent_profile.clone(),
        blob_store: tg.deps.stores.blob.clone(),
        channel_control: std::sync::Arc::clone(&tg.deps.channel_control),
        secret_vault: std::sync::Arc::clone(&tg.deps.secret_vault),
        deck_manager: std::sync::Arc::clone(&tg.deps.deck_manager),
        bind_display: tg.deps.runtime_config.admin_bind.to_string(),
    };
    let (admin_router, _spec) = baybo_gateway::api::admin::v1_router_and_spec();
    let router = admin_router
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            require_admin_token,
        ));
    (router, trace_store)
}

async fn get_tool_set(router: &axum::Router, hash: &str) -> (StatusCode, Value) {
    let req = auth(
        Request::builder()
            .method("GET")
            .uri(format!("/v1/traces/tool-sets/{hash}"))
            .body(Body::empty())
            .unwrap(),
    );
    let resp = router.clone().oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read body");
    let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, json)
}

fn sample_set() -> LlmToolSet {
    LlmToolSet::new(vec![
        LlmToolDefinition {
            name: "bash".into(),
            description: "Run a shell command.".into(),
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"],
            }),
        },
        LlmToolDefinition {
            name: "read_file".into(),
            description: "Read a workspace file.".into(),
            parameters_schema: serde_json::json!({ "type": "object" }),
        },
    ])
}

#[tokio::test]
async fn a_stored_tool_set_is_served_whole() {
    let (router, trace_store) = build_router().await;
    let set = sample_set();
    let row = set.to_row().expect("serialize set");
    trace_store.save_tool_set(&row).await.expect("save set");

    let (status, body) = get_tool_set(&router, row.hash.as_str()).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["hash"].as_str(), Some(row.hash.as_str()));

    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["name"].as_str(), Some("bash"));
    // The schema rides whole — the tab renders it verbatim, so a lossy
    // projection here would show the reader a tool the model never saw.
    assert_eq!(
        tools[0]["parameters_schema"]["properties"]["command"]["type"].as_str(),
        Some("string")
    );
}

#[tokio::test]
async fn an_unknown_hash_is_a_404_not_an_empty_set() {
    // 404, not `{ tools: [] }`: "the set is gone" and "the call offered no
    // tools" are different facts and the viewer says different things.
    let (router, _) = build_router().await;
    let absent = ToolSetHash::from_digest(&[9; 32]);
    let (status, _) = get_tool_set(&router, absent.as_str()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_malformed_hash_is_a_400() {
    // `ToolSetHash` only parses a 64-char lowercase hex digest, so garbage —
    // including an uppercased or short digest — is rejected at the path
    // extractor rather than turned into a store lookup that cannot hit.
    let (router, _) = build_router().await;
    for bad in [
        "not-a-hash",
        &"A".repeat(64),
        &"z".repeat(64),
        &"a".repeat(63),
    ] {
        let (status, _) = get_tool_set(&router, bad).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "for {bad:?}");
    }
}
