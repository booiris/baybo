//! The two `/v1/traces` surfaces that resolve what an `LlmCall` span only
//! references: its tool set, and where its input tokens went.
//!
//! `/v1/traces/tool-sets/{hash}` sits under `/v1/traces/`, one segment shape away from
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

/// A router plus the stores behind it, so a test can seed the very rows the
/// endpoints read.
async fn build_router_with_lifecycle() -> (
    axum::Router,
    std::sync::Arc<dyn TraceStore>,
    std::sync::Arc<baybo_turn::TurnLifecycle>,
) {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let trace_store = tg.deps.stores.trace.clone();
    let turn_lifecycle = std::sync::Arc::clone(&tg.deps.turn_lifecycle);

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
    (router, trace_store, turn_lifecycle)
}

/// The common case: only the router and the trace store are needed.
async fn build_router() -> (axum::Router, std::sync::Arc<dyn TraceStore>) {
    let (router, trace_store, _) = build_router_with_lifecycle().await;
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

// ── GET /v1/traces/{session_id}/spans/{span_id}/context ──────────────

/// Seed one `LlmCall` span with an inline input and a stored tool set, and
/// return the ids needed to ask for its context.
///
/// Inline rather than a `Persisted` ordinal reference on purpose: hydration
/// is exercised where it lives (`baybo-query`), and pinning it again here
/// would make this test fail for reasons that have nothing to do with the
/// endpoint.
async fn seed_llm_span(
    trace_store: &std::sync::Arc<dyn TraceStore>,
    turn_lifecycle: &std::sync::Arc<baybo_turn::TurnLifecycle>,
    session_id: &baybo_model::SessionId,
) -> baybo_model::SpanId {
    use baybo_model::{ChatMessage, ContentBlock};
    use baybo_trace::{
        LifecycleState, LlmCallBegin, LlmCallInputs, LlmCallResult, Span, SpanKind, Step, StepKind,
    };

    let turn = turn_lifecycle
        .start_turn(
            session_id.clone(),
            baybo_model::TriggerKind::User,
            baybo_turn::TurnInput::UserChat {
                content: vec![ContentBlock::Text("hi".into())],
            },
            None,
        )
        .await
        .expect("seed turn");

    let set = sample_set();
    let row = set.to_row().expect("serialize set");
    trace_store.save_tool_set(&row).await.expect("save set");

    let step_id = baybo_model::StepId::new();
    let now = chrono::Utc::now();
    trace_store
        .save_step(
            &Step {
                id: step_id,
                turn_id: turn.id,
                kind: StepKind::LlmIteration,
                started_at: now,
                ended_at: None,
                outcome: LifecycleState::Pending,
            }
            .to_row()
            .expect("step row"),
        )
        .await
        .expect("save step");

    let span_id = baybo_model::SpanId::new();
    trace_store
        .save_span(
            &Span {
                id: span_id,
                step_id,
                kind: SpanKind::LlmCall {
                    begin: LlmCallBegin {
                        model_id: "claude".into(),
                        provider: "anthropic".into(),
                        reasoning_effort: None,
                        input_messages: LlmCallInputs::Inline(vec![
                            ChatMessage::system(vec![ContentBlock::Text(
                                "you are a careful assistant".into(),
                            )]),
                            ChatMessage::user(vec![ContentBlock::Text(
                                "what broke the build?".into(),
                            )]),
                        ]),
                        temperature: None,
                        tools: Some(baybo_trace::LlmToolSetRef {
                            hash: row.hash.clone(),
                            count: set.tools.len(),
                        }),
                    },
                    result: Some(LlmCallResult {
                        input_tokens: 4_242,
                        ..Default::default()
                    }),
                },
                parallel_group: None,
                started_at: now,
                ended_at: None,
                outcome: LifecycleState::Pending,
                events: vec![],
            }
            .to_row()
            .expect("span row"),
        )
        .await
        .expect("save span");

    span_id
}

async fn get_span_context(
    router: &axum::Router,
    session_id: &str,
    span_id: &str,
) -> (StatusCode, Value) {
    let req = auth(
        Request::builder()
            .method("GET")
            .uri(format!("/v1/traces/{session_id}/spans/{span_id}/context"))
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

#[tokio::test]
async fn a_span_context_splits_the_input_into_parts() {
    let (router, trace_store, lifecycle) = build_router_with_lifecycle().await;
    let session_id = baybo_model::SessionId::from("ctx-session");
    let span_id = seed_llm_span(&trace_store, &lifecycle, &session_id).await;

    let (status, body) = get_span_context(&router, session_id.as_str(), &span_id.to_string()).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    // The provider's number is exact and passes through untouched; the split
    // beside it is a tokenizer estimate, and conflating the two is exactly
    // what the separate fields exist to prevent.
    assert_eq!(body["reported_input_tokens"].as_u64(), Some(4_242));
    assert!(body["estimated_total_tokens"].as_u64().unwrap() > 0);
    assert_ne!(
        body["estimated_total_tokens"].as_u64(),
        body["reported_input_tokens"].as_u64(),
        "the fixture's estimate should not coincidentally equal the reported total; \
         if it does this assertion is worthless and needs a different fixture"
    );

    let segments = body["segments"].as_array().expect("segments");
    let parts: Vec<&str> = segments
        .iter()
        .map(|s| s["part"].as_str().unwrap())
        .collect();
    // Tools precede the transcript on the wire, and the system row precedes
    // the question — the segment order IS the order the model saw.
    assert_eq!(parts, vec!["tools", "system_prompt", "user"]);
    assert!(
        segments.iter().all(|s| s["tokens"].as_u64().unwrap() > 0),
        "a zero-token segment would draw an invisible cell: {segments:?}"
    );
    assert_eq!(
        segments
            .iter()
            .map(|s| s["tokens"].as_u64().unwrap())
            .sum::<u64>(),
        body["estimated_total_tokens"].as_u64().unwrap(),
        "the total must be the sum of the parts, or the grid cannot add up"
    );
}

#[tokio::test]
async fn a_tool_span_has_no_context_to_break_down() {
    // 400, not an empty breakdown: a tool call sends no model input, and an
    // empty `segments` would render as "this call had no context".
    use baybo_trace::{LifecycleState, Span, SpanKind, Step, StepKind, ToolCallBegin};

    let (router, trace_store, lifecycle) = build_router_with_lifecycle().await;
    let session_id = baybo_model::SessionId::from("ctx-session-tool");
    let turn = lifecycle
        .start_turn(
            session_id.clone(),
            baybo_model::TriggerKind::User,
            baybo_turn::TurnInput::UserChat {
                content: vec![baybo_model::ContentBlock::Text("hi".into())],
            },
            None,
        )
        .await
        .expect("seed turn");

    let step_id = baybo_model::StepId::new();
    let now = chrono::Utc::now();
    trace_store
        .save_step(
            &Step {
                id: step_id,
                turn_id: turn.id,
                kind: StepKind::LlmIteration,
                started_at: now,
                ended_at: None,
                outcome: LifecycleState::Pending,
            }
            .to_row()
            .unwrap(),
        )
        .await
        .unwrap();
    let span_id = baybo_model::SpanId::new();
    trace_store
        .save_span(
            &Span {
                id: span_id,
                step_id,
                kind: SpanKind::ToolCall {
                    begin: ToolCallBegin {
                        tool_name: "bash".into(),
                        triggered_by: None,
                        params: serde_json::json!({}),
                    },
                    result: None,
                },
                parallel_group: None,
                started_at: now,
                ended_at: None,
                outcome: LifecycleState::Pending,
                events: vec![],
            }
            .to_row()
            .unwrap(),
        )
        .await
        .unwrap();

    let (status, _) = get_span_context(&router, session_id.as_str(), &span_id.to_string()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_unknown_span_id_is_a_404_and_a_malformed_one_a_400() {
    let (router, _, _) = build_router_with_lifecycle().await;
    let (status, _) = get_span_context(&router, "s", &baybo_model::SpanId::new().to_string()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = get_span_context(&router, "s", "not-a-ulid").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
