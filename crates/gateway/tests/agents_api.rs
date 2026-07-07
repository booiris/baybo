//! Integration coverage for the admin-side `/v1/agents` REST surface.
//!
//! Walks the CRUD contract from `docs/modules/agent-profiles.md`: the
//! seeded builtin is listed first and locked (400 on content update /
//! delete, avatar allowed), creates validate name/llm/avatar-blob, `PUT`
//! is a full content replace, and deletes are plain row removals.

use std::sync::Arc;

use axum::body::{self, Body};
use axum::http::{Request, StatusCode};
use baybo_gateway::test_support::build_test_deps;
use serde_json::{Value, json};
use tower::ServiceExt;

#[tokio::test]
async fn agents_api_round_trip() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let state = build_admin_state(&tg);
    let router = build_router(state);

    // ── 1. Fresh install lists exactly the locked builtin ───────────
    let list = get(&router, "/v1/agents", StatusCode::OK).await;
    let items = list["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "fresh DB has only the builtin: {items:?}");
    let builtin = &items[0];
    assert_eq!(builtin["id"].as_str(), Some("baybo"));
    assert_eq!(builtin["name"].as_str(), Some("baybo"));
    assert_eq!(builtin["builtin"].as_bool(), Some(true));
    assert_eq!(builtin["framework"].as_str(), Some("baybo"));
    // NULL = inherit-default fields are absent on the wire.
    for inherit in ["system_prompt", "llm", "avatar_blob_id"] {
        assert!(
            builtin.get(inherit).is_none(),
            "builtin {inherit} must be absent (inherit), got {builtin:?}",
        );
    }

    // ── 2. Create validates name / llm / avatar blob ─────────────────
    let err = post_expect(
        &router,
        "/v1/agents",
        json!({ "name": "   " }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(err["error"].as_str().unwrap_or("").contains("empty"));

    let err = post_expect(
        &router,
        "/v1/agents",
        json!({ "name": "Helper", "llm": "nope" }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(
        err["error"]
            .as_str()
            .unwrap_or("")
            .contains("unknown LLM entry")
    );

    let err = post_expect(
        &router,
        "/v1/agents",
        json!({ "name": "Helper", "avatar_blob_id": "sha256:feed.beef" }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(
        err["error"]
            .as_str()
            .unwrap_or("")
            .contains("unknown blob id")
    );

    // A real but non-image blob is rejected too.
    let text_blob = tg
        .deps
        .stores
        .blob
        .put(b"not an image", "text/plain", None)
        .await
        .expect("upload text blob");
    let err = post_expect(
        &router,
        "/v1/agents",
        json!({ "name": "Helper", "avatar_blob_id": text_blob.blob_id }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(
        err["error"]
            .as_str()
            .unwrap_or("")
            .contains("must be an image")
    );

    // ── 3. Create round-trips (trimmed name, image avatar, lists) ────
    let png_blob = tg
        .deps
        .stores
        .blob
        .put(&[0x89, b'P', b'N', b'G'], "image/png", None)
        .await
        .expect("upload png blob");
    let llm_entry = tg
        .deps
        .llm_pool
        .read()
        .entry_names()
        .first()
        .expect("test pool has one entry")
        .to_string();
    let created = post_expect(
        &router,
        "/v1/agents",
        json!({
            "name": "  Helper  ",
            "description": "test persona",
            "framework": "claude",
            "system_prompt": "Be terse.",
            "llm": llm_entry,
            "avatar_blob_id": png_blob.blob_id,
        }),
        StatusCode::OK,
    )
    .await;
    let agent_id = created["id"].as_str().expect("id").to_owned();
    assert_eq!(created["name"].as_str(), Some("Helper"), "name is trimmed");
    assert_eq!(created["builtin"].as_bool(), Some(false));
    assert_eq!(created["framework"].as_str(), Some("claude"));
    assert_eq!(created["llm"].as_str(), Some(llm_entry.as_str()));

    let fetched = get(&router, &format!("/v1/agents/{agent_id}"), StatusCode::OK).await;
    assert_eq!(fetched["name"].as_str(), Some("Helper"));
    assert_eq!(fetched["created_at"], created["created_at"]);

    // Duplicate names are rejected case-insensitively (builtin's too).
    let err = post_expect(
        &router,
        "/v1/agents",
        json!({ "name": "hELPer" }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(
        err["error"]
            .as_str()
            .unwrap_or("")
            .contains("already exists")
    );
    post_expect(
        &router,
        "/v1/agents",
        json!({ "name": "Baybo" }),
        StatusCode::BAD_REQUEST,
    )
    .await;

    // ── 4. Builtin is locked: content PUT and DELETE 400 ────────────
    let err = put_expect(
        &router,
        "/v1/agents/baybo",
        json!({ "name": "baybo", "description": "", "framework": "baybo" }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(err["error"].as_str().unwrap_or("").contains("read-only"));
    let err = delete_expect(&router, "/v1/agents/baybo", StatusCode::BAD_REQUEST).await;
    assert!(
        err["error"]
            .as_str()
            .unwrap_or("")
            .contains("cannot be deleted")
    );

    // …but its avatar is editable, and clearable.
    put_expect(
        &router,
        "/v1/agents/baybo/avatar",
        json!({ "blob_id": png_blob.blob_id }),
        StatusCode::NO_CONTENT,
    )
    .await;
    let listed = get(&router, "/v1/agents", StatusCode::OK).await;
    let builtin = &listed["items"].as_array().expect("items")[0];
    assert_eq!(
        builtin["avatar_blob_id"].as_str(),
        Some(png_blob.blob_id.as_str()),
    );
    put_expect(
        &router,
        "/v1/agents/baybo/avatar",
        json!({ "blob_id": null }),
        StatusCode::NO_CONTENT,
    )
    .await;

    // ── 5. PUT is a full replace: absent nullables reset to inherit ──
    put_expect(
        &router,
        &format!("/v1/agents/{agent_id}"),
        json!({ "name": "Helper 2", "description": "", "framework": "baybo" }),
        StatusCode::NO_CONTENT,
    )
    .await;
    let replaced = get(&router, &format!("/v1/agents/{agent_id}"), StatusCode::OK).await;
    assert_eq!(replaced["name"].as_str(), Some("Helper 2"));
    assert_eq!(replaced["framework"].as_str(), Some("baybo"));
    for reset in ["system_prompt", "llm"] {
        assert!(
            replaced.get(reset).is_none(),
            "full replace must reset {reset} to inherit, got {replaced:?}",
        );
    }
    assert_eq!(
        replaced["avatar_blob_id"].as_str(),
        Some(png_blob.blob_id.as_str()),
        "avatar is not touched by the content PUT",
    );

    // Renames conflict case-insensitively against other rows only: a
    // case-only self-rename is fine, taking another profile's name 400s.
    put_expect(
        &router,
        &format!("/v1/agents/{agent_id}"),
        json!({ "name": "HELPER 2", "description": "", "framework": "baybo" }),
        StatusCode::NO_CONTENT,
    )
    .await;
    let beta = post_expect(
        &router,
        "/v1/agents",
        json!({ "name": "Beta" }),
        StatusCode::OK,
    )
    .await;
    let beta_id = beta["id"].as_str().expect("id").to_owned();
    let err = put_expect(
        &router,
        &format!("/v1/agents/{beta_id}"),
        json!({ "name": "helper 2", "description": "", "framework": "baybo" }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(
        err["error"]
            .as_str()
            .unwrap_or("")
            .contains("already exists")
    );
    delete_expect(
        &router,
        &format!("/v1/agents/{beta_id}"),
        StatusCode::NO_CONTENT,
    )
    .await;

    // Unknown ids 404 across the surface.
    get(&router, "/v1/agents/missing", StatusCode::NOT_FOUND).await;
    put_expect(
        &router,
        "/v1/agents/missing",
        json!({ "name": "x", "description": "", "framework": "baybo" }),
        StatusCode::NOT_FOUND,
    )
    .await;
    delete_expect(&router, "/v1/agents/missing", StatusCode::NOT_FOUND).await;

    // ── 6. Delete removes the custom row ─────────────────────────────
    delete_expect(
        &router,
        &format!("/v1/agents/{agent_id}"),
        StatusCode::NO_CONTENT,
    )
    .await;
    get(
        &router,
        &format!("/v1/agents/{agent_id}"),
        StatusCode::NOT_FOUND,
    )
    .await;
    let list = get(&router, "/v1/agents", StatusCode::OK).await;
    assert_eq!(list["items"].as_array().expect("items").len(), 1);
}

// ── helpers ─────────────────────────────────────────────────────────

fn build_admin_state(
    tg: &baybo_gateway::test_support::TestGateway,
) -> baybo_gateway::server::AdminState {
    baybo_gateway::server::AdminState {
        config: Arc::clone(&tg.deps.config),
        config_path: tg.deps.config_path.clone(),
        session_manager: Arc::clone(&tg.deps.session_manager),
        job_lifecycle: Arc::clone(&tg.deps.job_lifecycle),
        cron_scheduler: Arc::clone(&tg.deps.cron_scheduler),
        trace_store: tg.deps.stores.trace.clone(),
        cost_store: tg.deps.stores.cost.clone(),
        query_api: Arc::new(baybo_query::QueryApi::new(
            tg.deps.session_manager.store(),
            Arc::clone(&tg.deps.job_lifecycle),
            tg.deps.stores.trace.clone(),
            tg.deps.stores.cost.clone(),
        )),
        skill_registry: Arc::clone(&tg.deps.skill_registry),
        tool_registry: Arc::clone(&tg.deps.tool_registry),
        channel_registry: Arc::clone(&tg.deps.channel_registry),
        llm_pool: tg.deps.llm_pool.clone(),
        supervisor: tg.deps.supervisor.clone(),
        config_reloader: tg.deps.config_reloader.clone(),
        log_buffer: Arc::clone(&tg.deps.log_buffer),
        channel_bot_store: tg.deps.stores.channel_bot.clone(),
        agent_profile_store: tg.deps.stores.agent_profile.clone(),
        blob_store: tg.deps.stores.blob.clone(),
        channel_control: Arc::clone(&tg.deps.channel_control),
        secret_vault: Arc::clone(&tg.deps.secret_vault),
        bind_display: tg.deps.runtime_config.admin_bind.to_string(),
    }
}

fn build_router(state: baybo_gateway::server::AdminState) -> axum::Router {
    let (router, _spec) = baybo_gateway::api::admin::v1_router_and_spec();
    router.with_state(state)
}

async fn request(
    router: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    expected: StatusCode,
) -> Value {
    let builder = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(v) => builder
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = router
        .clone()
        .oneshot(request)
        .await
        .expect("router responds");
    assert_eq!(
        response.status(),
        expected,
        "{method} {uri} expected {expected:?} got {:?}",
        response.status(),
    );
    let bytes = body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .expect("body bytes");
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).expect("response is json")
}

async fn get(router: &axum::Router, uri: &str, expected: StatusCode) -> Value {
    request(router, "GET", uri, None, expected).await
}

async fn post_expect(router: &axum::Router, uri: &str, body: Value, expected: StatusCode) -> Value {
    request(router, "POST", uri, Some(body), expected).await
}

async fn put_expect(router: &axum::Router, uri: &str, body: Value, expected: StatusCode) -> Value {
    request(router, "PUT", uri, Some(body), expected).await
}

async fn delete_expect(router: &axum::Router, uri: &str, expected: StatusCode) -> Value {
    request(router, "DELETE", uri, None, expected).await
}
