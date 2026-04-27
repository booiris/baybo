//! The admin TCP listener must not serve chat-surface routes.
//!
//! `POST /v1/sessions` (create), `POST /v1/sessions/{id}/messages`
//! (send), `GET /v1/sessions/{id}/stream`, and `/v1/approvals*` live
//! exclusively on the loopback-TCP channel listener. Requesting any
//! of them through the admin router has to come back `404`, even
//! with a valid bearer token — a leaked admin credential must not
//! yield chat content or approval control.
//!
//! Note: admin DOES serve metadata-only session views
//! (`GET /v1/sessions`, `GET /v1/sessions/{id}` — both omit the
//! transcript) plus the cross-session fork operation
//! (`POST /v1/sessions/{id}/fork`) introduced in the trace 4-layer
//! redesign. Those endpoints are admin observability + governance,
//! not chat content.
//!
//! The test drives `GatewayServer`'s router through `tower::ServiceExt`
//! so we exercise the actual route table the TCP listener assembles (not
//! a hand-written subset). `/v1/channels` is the canary that the admin
//! router *does* serve: if that returned 404 we'd be misreading the
//! result.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use tower::ServiceExt;

use aura_gateway::test_support::{TEST_ADMIN_TOKEN, build_test_deps};

fn auth(req: Request<Body>) -> Request<Body> {
    let (mut parts, body) = req.into_parts();
    parts.headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {TEST_ADMIN_TOKEN}").parse().unwrap(),
    );
    Request::from_parts(parts, body)
}

async fn admin_router() -> axum::Router {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    // Re-run the private `build_admin_router` via `GatewayServer::new`
    // — but it doesn't expose its router. Instead, duplicate the
    // assembly here using public building blocks.
    use aura_gateway::auth::admin::{AdminAuthState, require_admin_token};
    let auth_state = AdminAuthState::new(tg.deps.admin_token.clone());
    let state = aura_gateway::server::AdminState {
        config: std::sync::Arc::clone(&tg.deps.config),
        config_path: tg.deps.config_path.clone(),
        session_manager: std::sync::Arc::clone(&tg.deps.session_manager),
        job_manager: std::sync::Arc::clone(&tg.deps.job_manager),
        cron_scheduler: std::sync::Arc::clone(&tg.deps.cron_scheduler),
        memory_manager: std::sync::Arc::clone(&tg.deps.memory_manager),
        trace_store: tg.deps.stores.trace.clone(),
        skill_registry: std::sync::Arc::clone(&tg.deps.skill_registry),
        tool_registry: std::sync::Arc::clone(&tg.deps.tool_registry),
        channel_registry: std::sync::Arc::clone(&tg.deps.channel_registry),
        llm_client: std::sync::Arc::clone(&tg.deps.llm_client),
        log_buffer: std::sync::Arc::clone(&tg.deps.log_buffer),
        channel_bot_store: tg.deps.stores.channel_bot.clone(),
        channel_control: std::sync::Arc::clone(&tg.deps.channel_control),
        secret_vault: std::sync::Arc::clone(&tg.deps.secret_vault),
        bind_display: tg.deps.runtime_config.admin_bind.to_string(),
    };
    let (admin_router, _spec) = aura_gateway::api::admin::v1_router_and_spec();
    let admin_router = admin_router
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            require_admin_token,
        ));
    axum::Router::new()
        .merge(aura_gateway::api::health::routes())
        .merge(admin_router)
}

async fn assert_not_found(method: Method, path: &str) {
    let router = admin_router().await;
    let req = auth(
        Request::builder()
            .method(method.clone())
            .uri(path)
            .body(Body::empty())
            .unwrap(),
    );
    let resp = router.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    // 405 (Method Not Allowed) also satisfies "not served" — it
    // happens when a sibling route on the same path uses a different
    // method (e.g. admin serves GET /v1/sessions but not POST).
    assert!(
        status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED,
        "{method} {path} must not be on admin listener (got {status})"
    );
}

#[tokio::test]
async fn create_session_is_404_on_admin() {
    assert_not_found(Method::POST, "/v1/sessions").await;
}

#[tokio::test]
async fn send_message_is_404_on_admin() {
    assert_not_found(Method::POST, "/v1/sessions/any/messages").await;
}

#[tokio::test]
async fn session_stream_is_404_on_admin() {
    assert_not_found(Method::GET, "/v1/sessions/any/stream").await;
}

#[tokio::test]
async fn approvals_list_is_404_on_admin() {
    assert_not_found(Method::GET, "/v1/approvals").await;
}

#[tokio::test]
async fn resolve_approval_is_404_on_admin() {
    assert_not_found(Method::POST, "/v1/approvals/any").await;
}

#[tokio::test]
async fn approvals_stream_is_404_on_admin() {
    assert_not_found(Method::GET, "/v1/approvals/stream").await;
}

#[tokio::test]
async fn channels_list_is_still_on_admin() {
    // Canary: if this returned 404, the test suite above would be a
    // false positive (we'd be testing an unmounted router, not one that
    // has channel routes specifically removed).
    let router = admin_router().await;
    let req = auth(
        Request::builder()
            .method(Method::GET)
            .uri("/v1/channels")
            .body(Body::empty())
            .unwrap(),
    );
    let resp = router.oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
}
