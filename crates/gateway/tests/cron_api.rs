//! Integration coverage for the admin-side `/v1/cron` REST surface:
//! pause / resume, and the recycle bin (soft delete → listed only under
//! `?deleted=true`, still resolvable by id, restorable).

use std::sync::Arc;

use axum::body::{self, Body};
use axum::http::{Request, StatusCode};
use baybo_gateway::test_support::build_test_deps;
use serde_json::{Value, json};
use tower::ServiceExt;

#[tokio::test]
async fn cron_pause_resume_and_recycle_bin_round_trip() {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let router = build_router(build_admin_state(&tg));

    // ── 1. Create: enabled, scheduled, live ─────────────────────────
    let created = post_expect(
        &router,
        "/v1/cron",
        json!({
            "schedule": "0 9 * * *",
            "user_id": "owner",
            "title": "Morning digest",
            "text": "Summarize the news",
            "timezone": "UTC",
        }),
        StatusCode::CREATED,
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();
    assert_eq!(created["status"].as_str(), Some("enabled"));
    assert!(!created["next_trigger_at"].is_null(), "{created:?}");
    assert!(
        created.get("deleted_at").is_none(),
        "a live job carries no deleted_at: {created:?}",
    );

    let bad = post_expect(
        &router,
        "/v1/cron",
        json!({
            "schedule": "not a cron expression",
            "user_id": "owner",
            "title": "Broken",
            "text": "…",
            "timezone": "UTC",
        }),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert!(
        bad["error"]
            .as_str()
            .unwrap_or("")
            .contains("invalid schedule"),
        "{bad:?}",
    );

    // ── 2. Pause clears the next trigger; resume recomputes it ──────
    post_expect(
        &router,
        &format!("/v1/cron/{id}/pause"),
        json!({}),
        StatusCode::NO_CONTENT,
    )
    .await;
    let paused = get(&router, &format!("/v1/cron/{id}"), StatusCode::OK).await;
    assert_eq!(paused["status"].as_str(), Some("disabled"));
    assert!(
        paused["next_trigger_at"].is_null(),
        "a paused job has no next trigger: {paused:?}",
    );
    assert_eq!(
        listed_ids(&router, false).await,
        vec![id.clone()],
        "pausing keeps the job in the live list",
    );

    post_expect(
        &router,
        &format!("/v1/cron/{id}/resume"),
        json!({}),
        StatusCode::NO_CONTENT,
    )
    .await;
    let resumed = get(&router, &format!("/v1/cron/{id}"), StatusCode::OK).await;
    assert_eq!(resumed["status"].as_str(), Some("enabled"));
    assert!(!resumed["next_trigger_at"].is_null(), "{resumed:?}");

    // ── 3. Delete is a soft delete: out of the live list, into the bin,
    //       still resolvable by id ────────────────────────────────────
    delete_expect(&router, &format!("/v1/cron/{id}"), StatusCode::NO_CONTENT).await;
    assert!(
        listed_ids(&router, false).await.is_empty(),
        "the default list never carries a deleted job",
    );
    assert_eq!(listed_ids(&router, true).await, vec![id.clone()]);
    let deleted = get(&router, &format!("/v1/cron/{id}"), StatusCode::OK).await;
    assert!(
        !deleted["deleted_at"].is_null(),
        "a binned job reports when it was deleted: {deleted:?}",
    );
    assert_eq!(
        deleted["status"].as_str(),
        Some("enabled"),
        "deletion is orthogonal to status",
    );

    // ── 4. Pause / resume do not act on a job in the bin: reporting
    //       success would promise a fire `list_due` can never produce ─
    for action in ["pause", "resume"] {
        post_expect(
            &router,
            &format!("/v1/cron/{id}/{action}"),
            json!({}),
            StatusCode::NOT_FOUND,
        )
        .await;
    }

    // ── 5. Restore brings it back, live and scheduled from now ──────
    post_expect(
        &router,
        &format!("/v1/cron/{id}/restore"),
        json!({}),
        StatusCode::NO_CONTENT,
    )
    .await;
    assert_eq!(listed_ids(&router, false).await, vec![id.clone()]);
    assert!(listed_ids(&router, true).await.is_empty());
    let restored = get(&router, &format!("/v1/cron/{id}"), StatusCode::OK).await;
    assert!(
        restored.get("deleted_at").is_none(),
        "restore clears deleted_at: {restored:?}",
    );
    assert_eq!(restored["status"].as_str(), Some("enabled"));
    assert!(!restored["next_trigger_at"].is_null(), "{restored:?}");

    // ── 6. Unknown ids 404 across the whole surface ─────────────────
    for action in ["pause", "resume", "restore"] {
        post_expect(
            &router,
            &format!("/v1/cron/missing/{action}"),
            json!({}),
            StatusCode::NOT_FOUND,
        )
        .await;
    }
    delete_expect(&router, "/v1/cron/missing", StatusCode::NOT_FOUND).await;
    get(&router, "/v1/cron/missing", StatusCode::NOT_FOUND).await;
}

// ── helpers ─────────────────────────────────────────────────────────

async fn listed_ids(router: &axum::Router, deleted: bool) -> Vec<String> {
    let uri = if deleted {
        "/v1/cron?deleted=true"
    } else {
        "/v1/cron"
    };
    get(router, uri, StatusCode::OK).await["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|j| j["id"].as_str().unwrap_or_default().to_owned())
        .collect()
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
            .expect("build request"),
        None => builder.body(Body::empty()).expect("build request"),
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

async fn delete_expect(router: &axum::Router, uri: &str, expected: StatusCode) -> Value {
    request(router, "DELETE", uri, None, expected).await
}
