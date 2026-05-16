//! `/v1/llm/*` admin-router integration tests.
//!
//! Constructs the admin router by hand (same pattern as
//! `logs_endpoint.rs`), points it at a tempdir-backed `aura.json` plus
//! the test gateway's libsql cost store, then drives each endpoint via
//! `tower::ServiceExt::oneshot`.
//!
//! Why these exist: `update_model` / `set_default` rewrite `aura.json`
//! and the round-trip + validation rules are subtle (`Option<Option<T>>`
//! "absent vs cleared", `context_window: Some(0)` → 400, default-llm
//! cross-check). The original PR shipped without coverage and these
//! pin the contracts so a future `derive(Deserialize)` change can't
//! silently collapse them.

use std::net::SocketAddr;
use std::sync::Arc;

use aura_config::{AuraConfig, LlmEntry};
use aura_gateway::test_support::{TEST_ADMIN_TOKEN, build_test_deps};
use aura_model::{JobId, MicroUsd, SessionId, SpanId};
use aura_cost::CostRecord;
use axum::body::{self, Body};
use axum::http::{Request, StatusCode, header};
use chrono::Utc;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

fn auth(req: Request<Body>) -> Request<Body> {
    let (mut parts, body) = req.into_parts();
    parts.headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {TEST_ADMIN_TOKEN}").parse().unwrap(),
    );
    Request::from_parts(parts, body)
}

/// Drop the test gateway's default config + path on disk, return a
/// configured admin router and the tempdir / path so callers can
/// re-read the file after a mutation.
async fn router_with_seed_config(seed: AuraConfig) -> (axum::Router, TempDir, std::path::PathBuf) {
    let tg = build_test_deps(SocketAddr::from(([127, 0, 0, 1], 0))).await;
    let cfg_dir = tempfile::tempdir().expect("config tempdir");
    let cfg_path = cfg_dir.path().join("aura.json");
    seed.write_to_file(&cfg_path)
        .await
        .expect("write seed aura.json");

    use aura_gateway::auth::admin::{AdminAuthState, require_admin_token};
    let auth_state = AdminAuthState::new(tg.deps.admin_token.clone());
    let state = aura_gateway::server::AdminState {
        config: Arc::new(seed),
        config_path: Some(cfg_path.clone()),
        session_manager: Arc::clone(&tg.deps.session_manager),
        job_lifecycle: Arc::clone(&tg.deps.job_lifecycle),
        cron_scheduler: Arc::clone(&tg.deps.cron_scheduler),
        memory_manager: Arc::clone(&tg.deps.memory_manager),
        trace_store: tg.deps.stores.trace.clone(),
        cost_store: tg.deps.stores.cost.clone(),
        query_api: Arc::new(aura_query::QueryApi::new(
            tg.deps.session_manager.store(),
            Arc::clone(&tg.deps.job_lifecycle),
            tg.deps.stores.trace.clone(),
            tg.deps.stores.cost.clone(),
        )),
        skill_registry: Arc::clone(&tg.deps.skill_registry),
        tool_registry: Arc::clone(&tg.deps.tool_registry),
        channel_registry: Arc::clone(&tg.deps.channel_registry),
        llm_client: tg.deps.llm_client.clone(),
        log_buffer: Arc::clone(&tg.deps.log_buffer),
        channel_bot_store: tg.deps.stores.channel_bot.clone(),
        channel_control: Arc::clone(&tg.deps.channel_control),
        secret_vault: Arc::clone(&tg.deps.secret_vault),
        channel_tokens: tg.deps.channel_tokens.clone(),
        web_chat_tokens: Arc::new(dashmap::DashMap::new()),
        bind_display: tg.deps.runtime_config.admin_bind.to_string(),
    };
    let (admin_router, _spec) = aura_gateway::api::admin::v1_router_and_spec();
    let admin_router = admin_router
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            require_admin_token,
        ));
    let router = axum::Router::new().merge(admin_router);
    // tg.tempdir keeps libsql alive — leak it so the cost store keeps
    // pointing at a live file for the duration of the test. Returning
    // the cfg_dir guarantees `cfg_path` stays live too.
    Box::leak(Box::new(tg));
    (router, cfg_dir, cfg_path)
}

fn entry(name: &str, provider: &str, model: &str) -> LlmEntry {
    LlmEntry {
        name: name.into(),
        provider: provider.into(),
        model: model.into(),
        api_key_env: None,
        base_url: None,
        supports_vision: None,
        context_window: None,
        pricing: None,
        reasoning_effort: None,
    }
}

fn seed_two_entries() -> AuraConfig {
    AuraConfig {
        llm: vec![
            entry("primary", "openai", "gpt-4o"),
            entry("secondary", "anthropic", "claude-sonnet-4-6"),
        ],
        default_llm: "primary".into(),
        ..Default::default()
    }
}

async fn read_json(resp: axum::response::Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = body::to_bytes(resp.into_body(), 256 * 1024)
        .await
        .expect("collect body");
    let value: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("parse json")
    };
    (status, value)
}

// ── list_models ───────────────────────────────────────────────────────

#[tokio::test]
async fn list_models_returns_default_and_effective_fields() {
    let (router, _dir, _path) = router_with_seed_config(seed_two_entries()).await;
    let req = auth(
        Request::builder()
            .method("GET")
            .uri("/v1/llm/models")
            .body(Body::empty())
            .unwrap(),
    );
    let (status, body) = read_json(router.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["default_name"], "primary");
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2);

    let primary = &items[0];
    assert_eq!(primary["name"], "primary");
    assert_eq!(primary["is_default"], true);
    // Factory default for openai with no snapshot hit is the constant
    // we centralised in `aura_llm::factory_defaults_for`. The dashboard
    // must mirror it; a snapshot hit will only widen the value.
    assert!(primary["effective_context_window"].as_u64().unwrap() >= 128_000);
    assert_eq!(primary["effective_supports_vision"], true);
    assert_eq!(primary["context_window_override"], Value::Null);

    let secondary = &items[1];
    assert_eq!(secondary["name"], "secondary");
    assert_eq!(secondary["is_default"], false);
}

// ── update_model ─────────────────────────────────────────────────────

#[tokio::test]
async fn update_model_persists_overrides_to_disk() {
    let (router, _dir, path) = router_with_seed_config(seed_two_entries()).await;
    let body = json!({
        "context_window": 64_000,
        "supports_vision": false,
        "pricing": {
            "input_per_1m_tokens": 2_500_000_i64,
            "output_per_1m_tokens": 10_000_000_i64,
        }
    });
    let req = auth(
        Request::builder()
            .method("PUT")
            .uri("/v1/llm/models/primary")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    );
    let (status, resp) = read_json(router.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["requires_restart"], true);
    assert_eq!(resp["path"], "llm[primary]");

    let on_disk = AuraConfig::load_from_file(&path).await.expect("reload");
    let primary = on_disk.llm_entry("primary").expect("primary present");
    assert_eq!(primary.context_window, Some(64_000));
    assert_eq!(primary.supports_vision, Some(false));
    let pricing = primary.pricing.expect("pricing override saved");
    assert_eq!(
        pricing.input_per_1m_tokens,
        Some(MicroUsd::from_micros(2_500_000))
    );
    assert_eq!(
        pricing.output_per_1m_tokens,
        Some(MicroUsd::from_micros(10_000_000))
    );
    assert!(pricing.cached_input_per_1m_tokens.is_none());
}

#[tokio::test]
async fn update_model_clears_override_on_explicit_null() {
    // Seed already-present overrides so we can prove that an explicit
    // `null` clears them. `Option<Option<T>>` is the only way the
    // backend can tell "absent (keep)" from "present-as-null (clear)".
    let mut seed = seed_two_entries();
    seed.llm[0].context_window = Some(64_000);
    seed.llm[0].supports_vision = Some(false);

    let (router, _dir, path) = router_with_seed_config(seed).await;
    let body = json!({
        "context_window": null,
        "supports_vision": null,
    });
    let req = auth(
        Request::builder()
            .method("PUT")
            .uri("/v1/llm/models/primary")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    );
    let (status, _) = read_json(router.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    let on_disk = AuraConfig::load_from_file(&path).await.expect("reload");
    let primary = on_disk.llm_entry("primary").expect("primary present");
    assert_eq!(primary.context_window, None);
    assert_eq!(primary.supports_vision, None);
}

#[tokio::test]
async fn update_model_rejects_zero_context_window() {
    let (router, _dir, path) = router_with_seed_config(seed_two_entries()).await;
    let body = json!({ "context_window": 0 });
    let req = auth(
        Request::builder()
            .method("PUT")
            .uri("/v1/llm/models/primary")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    );
    let (status, _) = read_json(router.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let on_disk = AuraConfig::load_from_file(&path).await.expect("reload");
    assert_eq!(on_disk.llm_entry("primary").unwrap().context_window, None);
}

#[tokio::test]
async fn update_model_returns_404_for_unknown_name() {
    let (router, _dir, _path) = router_with_seed_config(seed_two_entries()).await;
    let req = auth(
        Request::builder()
            .method("PUT")
            .uri("/v1/llm/models/nope")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    );
    let (status, _) = read_json(router.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── set_default ──────────────────────────────────────────────────────

#[tokio::test]
async fn set_default_persists_and_rejects_unknown_name() {
    let (router, _dir, path) = router_with_seed_config(seed_two_entries()).await;

    // Happy path.
    let req = auth(
        Request::builder()
            .method("PUT")
            .uri("/v1/llm/default")
            .header("content-type", "application/json")
            .body(Body::from(json!({ "name": "secondary" }).to_string()))
            .unwrap(),
    );
    let (status, resp) = read_json(router.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["path"], "default-llm");
    let on_disk = AuraConfig::load_from_file(&path).await.expect("reload");
    assert_eq!(on_disk.default_llm, "secondary");

    // Rejection: unknown name. Build a fresh router because oneshot
    // consumes the original.
    let (router, _dir, path) = router_with_seed_config(seed_two_entries()).await;
    let req = auth(
        Request::builder()
            .method("PUT")
            .uri("/v1/llm/default")
            .header("content-type", "application/json")
            .body(Body::from(json!({ "name": "nope" }).to_string()))
            .unwrap(),
    );
    let (status, _) = read_json(router.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let on_disk = AuraConfig::load_from_file(&path).await.expect("reload");
    assert_eq!(
        on_disk.default_llm, "primary",
        "default unchanged on rejection"
    );
}

// ── get_usage ────────────────────────────────────────────────────────

fn cost_record(model: &str, input: usize, output: usize, cost: MicroUsd) -> CostRecord {
    CostRecord {
        user_id: "u-test".into(),
        session_id: SessionId::from("sess-1"),
        job_id: JobId::new(),
        span_id: SpanId::new(),
        model: model.into(),
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: 0,
        cache_creation_input_tokens: 0,
        cost_usd: cost,
        timestamp: Utc::now(),
    }
}

#[tokio::test]
async fn get_usage_aggregates_by_model() {
    // Build the test gateway directly so we can seed the cost store
    // before mounting the router. Re-using `router_with_seed_config`
    // would seal the deps inside the leak.
    let tg = build_test_deps(SocketAddr::from(([127, 0, 0, 1], 0))).await;
    tg.deps
        .stores
        .cost
        .record(&cost_record(
            "gpt-4o",
            100,
            50,
            MicroUsd::from_micros(1_500),
        ))
        .await
        .unwrap();
    tg.deps
        .stores
        .cost
        .record(&cost_record(
            "gpt-4o",
            200,
            80,
            MicroUsd::from_micros(3_500),
        ))
        .await
        .unwrap();
    tg.deps
        .stores
        .cost
        .record(&cost_record(
            "claude-sonnet-4-6",
            300,
            120,
            MicroUsd::from_micros(7_000),
        ))
        .await
        .unwrap();

    let cfg_dir = tempfile::tempdir().expect("config tempdir");
    let cfg_path = cfg_dir.path().join("aura.json");
    seed_two_entries()
        .write_to_file(&cfg_path)
        .await
        .expect("write seed");

    use aura_gateway::auth::admin::{AdminAuthState, require_admin_token};
    let auth_state = AdminAuthState::new(tg.deps.admin_token.clone());
    let state = aura_gateway::server::AdminState {
        config: Arc::new(seed_two_entries()),
        config_path: Some(cfg_path.clone()),
        session_manager: Arc::clone(&tg.deps.session_manager),
        job_lifecycle: Arc::clone(&tg.deps.job_lifecycle),
        cron_scheduler: Arc::clone(&tg.deps.cron_scheduler),
        memory_manager: Arc::clone(&tg.deps.memory_manager),
        trace_store: tg.deps.stores.trace.clone(),
        cost_store: tg.deps.stores.cost.clone(),
        query_api: Arc::new(aura_query::QueryApi::new(
            tg.deps.session_manager.store(),
            Arc::clone(&tg.deps.job_lifecycle),
            tg.deps.stores.trace.clone(),
            tg.deps.stores.cost.clone(),
        )),
        skill_registry: Arc::clone(&tg.deps.skill_registry),
        tool_registry: Arc::clone(&tg.deps.tool_registry),
        channel_registry: Arc::clone(&tg.deps.channel_registry),
        llm_client: tg.deps.llm_client.clone(),
        log_buffer: Arc::clone(&tg.deps.log_buffer),
        channel_bot_store: tg.deps.stores.channel_bot.clone(),
        channel_control: Arc::clone(&tg.deps.channel_control),
        secret_vault: Arc::clone(&tg.deps.secret_vault),
        channel_tokens: tg.deps.channel_tokens.clone(),
        web_chat_tokens: Arc::new(dashmap::DashMap::new()),
        bind_display: tg.deps.runtime_config.admin_bind.to_string(),
    };
    let (admin_router, _spec) = aura_gateway::api::admin::v1_router_and_spec();
    let admin_router = admin_router
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            require_admin_token,
        ));
    let router = axum::Router::new().merge(admin_router);
    Box::leak(Box::new(tg));

    let req = auth(
        Request::builder()
            .method("GET")
            .uri("/v1/llm/usage")
            .body(Body::empty())
            .unwrap(),
    );
    let (status, body) = read_json(router.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2);
    let primary = items.iter().find(|i| i["name"] == "primary").unwrap();
    assert_eq!(primary["call_count"], 2);
    assert_eq!(primary["input_tokens"], 300);
    assert_eq!(primary["output_tokens"], 130);
    assert_eq!(primary["cost_micro_usd"], 5_000);
    let secondary = items.iter().find(|i| i["name"] == "secondary").unwrap();
    assert_eq!(secondary["call_count"], 1);
    assert_eq!(secondary["cost_micro_usd"], 7_000);
}

#[tokio::test]
async fn get_usage_rejects_inverted_range() {
    let (router, _dir, _path) = router_with_seed_config(seed_two_entries()).await;
    let now = Utc::now();
    let earlier = now - chrono::Duration::hours(1);
    let uri = format!(
        "/v1/llm/usage?since={}&until={}",
        urlencoding(&now.to_rfc3339()),
        urlencoding(&earlier.to_rfc3339()),
    );
    let req = auth(
        Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .unwrap(),
    );
    let (status, _) = read_json(router.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

fn urlencoding(s: &str) -> String {
    // Light-weight: only percent-encode the characters that actually
    // need it inside an RFC 3339 timestamp (`:` and `+`).
    s.replace(':', "%3A").replace('+', "%2B")
}
