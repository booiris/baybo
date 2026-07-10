//! Integration coverage for the admin-side `/v1/agents` REST surface.
//!
//! Walks the CRUD contract from `docs/modules/agent-profiles.md`: the
//! seeded builtin is listed first and locked (400 on content update /
//! delete, avatar allowed), creates validate name/llm/avatar-blob, `PUT`
//! is a full content replace, and deletes are plain row removals.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{self, Body};
use axum::http::{Request, StatusCode};
use baybo_agent::{LlmClientPool, LlmPoolHandle};
use baybo_gateway::test_support::build_test_deps;
use baybo_llm::{CostHooks, LlmProviderConfig, LlmProviderRegistry};
use baybo_model::LlmEntryName;
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

// `allowed_models` set validation + `reasoning_effort` validation, plus
// the pin-must-be-a-member rule (needs a second pool entry, hence the
// dedicated two-entry admin state — see `build_admin_state_two_llms`).
#[tokio::test]
async fn agent_model_set_and_effort_validation() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let (state, entry_a, entry_b) = build_admin_state_two_llms(&tg);
    let router = build_router(state);

    // ── 1. allowed_models + reasoning_effort round-trip ────────────────
    let created = post_expect(
        &router,
        "/v1/agents",
        json!({
            "name": "Restricted",
            "llm": entry_a,
            "allowed_models": [entry_a],
            "reasoning_effort": "high",
        }),
        StatusCode::OK,
    )
    .await;
    let agent_id = created["id"].as_str().expect("id").to_owned();
    assert_eq!(
        created["allowed_models"]
            .as_array()
            .expect("allowed_models"),
        &vec![Value::String(entry_a.clone())],
    );
    assert_eq!(created["reasoning_effort"].as_str(), Some("high"));

    let fetched = get(&router, &format!("/v1/agents/{agent_id}"), StatusCode::OK).await;
    assert_eq!(
        fetched["allowed_models"]
            .as_array()
            .expect("allowed_models"),
        &vec![Value::String(entry_a.clone())],
    );
    assert_eq!(fetched["reasoning_effort"].as_str(), Some("high"));

    // ── 1b. empty-string reasoning_effort clears to None ────────────────
    let cleared_effort = post_expect(
        &router,
        "/v1/agents",
        json!({ "name": "No Effort", "reasoning_effort": "" }),
        StatusCode::OK,
    )
    .await;
    assert!(
        cleared_effort.get("reasoning_effort").is_none(),
        "empty-string reasoning_effort must clear to absent, got {cleared_effort:?}",
    );

    // ── 2. unknown allowed_models member → 400 ──────────────────────────
    let err = post_expect(
        &router,
        "/v1/agents",
        json!({ "name": "Bad Set", "allowed_models": ["nope"] }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(
        err["error"]
            .as_str()
            .unwrap_or("")
            .contains("unknown LLM entry"),
        "expected unknown-entry error, got {err:?}",
    );

    // ── 2b. empty-string member → 400 ("must not be empty") ─────────────
    let err = post_expect(
        &router,
        "/v1/agents",
        json!({ "name": "Empty Member", "allowed_models": [""] }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(
        err["error"]
            .as_str()
            .unwrap_or("")
            .contains("must not be empty"),
        "expected empty-member error, got {err:?}",
    );

    // ── 3. pin not a member of the set → 400 (needs two real entries) ───
    let err = post_expect(
        &router,
        "/v1/agents",
        json!({ "name": "Mismatched", "llm": entry_a, "allowed_models": [entry_b] }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(
        err["error"]
            .as_str()
            .unwrap_or("")
            .contains("is not in allowed_models"),
        "expected pin/set mismatch error, got {err:?}",
    );

    // ── 4. unknown reasoning_effort → 400 listing legal values ──────────
    let err = post_expect(
        &router,
        "/v1/agents",
        json!({ "name": "Bad Effort", "reasoning_effort": "ultra" }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(
        err["error"].as_str().unwrap_or("").contains("minimal"),
        "expected legal-values listing, got {err:?}",
    );

    // ── 5. full-replace PUT without the two fields resets them ──────────
    put_expect(
        &router,
        &format!("/v1/agents/{agent_id}"),
        json!({ "name": "Restricted", "description": "", "framework": "baybo" }),
        StatusCode::NO_CONTENT,
    )
    .await;
    let after_put = get(&router, &format!("/v1/agents/{agent_id}"), StatusCode::OK).await;
    assert!(
        after_put.get("allowed_models").is_none(),
        "allowed_models must reset to absent, got {after_put:?}",
    );
    assert!(
        after_put.get("reasoning_effort").is_none(),
        "reasoning_effort must reset to absent, got {after_put:?}",
    );

    // ── 6. duplicates collapse, order preserved ──────────────────────────
    let created2 = post_expect(
        &router,
        "/v1/agents",
        json!({ "name": "Deduped", "allowed_models": [entry_b, entry_a, entry_b] }),
        StatusCode::OK,
    )
    .await;
    assert_eq!(
        created2["allowed_models"]
            .as_array()
            .expect("allowed_models"),
        &vec![
            Value::String(entry_b.clone()),
            Value::String(entry_a.clone())
        ],
        "duplicate allowed_models entries collapse, first-seen order preserved",
    );
}

// ── helpers ─────────────────────────────────────────────────────────

/// Build an `AdminState` whose LLM pool has two live entries (the
/// harness's original stub plus a freshly built second stub client), so
/// tests can exercise membership rules end-to-end (`validate_llm_pin`
/// keys off `state.llm_pool`, not `state.config`) — a bare unknown name
/// only proves the "not configured" 400, not the "configured but not a
/// set member" 400. `build_test_deps` itself stays single-entry (it's
/// shared by every gateway test file); this helper only swaps `llm_pool`
/// on a state built from the same `tg`, so it can't affect other tests.
fn build_admin_state_two_llms(
    tg: &baybo_gateway::test_support::TestGateway,
) -> (baybo_gateway::server::AdminState, String, String) {
    let original_name = tg
        .deps
        .llm_pool
        .read()
        .entry_names()
        .first()
        .expect("test pool has one entry")
        .clone();
    let original_client = tg.deps.llm_pool.read().default_client();

    let registry = LlmProviderRegistry::with_default_providers();
    let second_client = registry
        .create_client(
            &LlmProviderConfig {
                provider: "openai".into(),
                api_key: Some("sk-test-placeholder".into()),
                base_url: None,
                model: "gpt-4o-second-stub".into(),
                supports_vision: None,
                context_window: None,
                pricing: None,
                reasoning_effort: None,
                vault: None,
                proxy: None,
            },
            None,
            CostHooks::passthrough(),
        )
        .expect("second stub LLM client");
    let second_name = LlmEntryName::from(second_client.model_info().id.clone());

    let mut clients = HashMap::new();
    clients.insert(original_name.clone(), original_client);
    clients.insert(second_name.clone(), second_client);
    let llm_pool: LlmPoolHandle = Arc::new(parking_lot::RwLock::new(Arc::new(
        LlmClientPool::new(clients, original_name.clone())
            .expect("two-entry stub pool default present"),
    )));

    let mut state = build_admin_state(tg);
    state.llm_pool = llm_pool;
    (state, original_name.to_string(), second_name.to_string())
}

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
