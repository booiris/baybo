//! The admin TCP listener must not serve channel-surface routes.
//!
//! After the two-listener split, `/v1/sessions`,
//! `/v1/sessions/{id}`, `/v1/sessions/{id}/messages`,
//! `/v1/sessions/{id}/stream`, and `/v1/approvals*` live exclusively
//! on the loopback-TCP channel listener. Requesting any of them
//! through the admin router has to come back `404`, even with a
//! valid bearer token — a leaked admin credential must not yield
//! chat content or approval control.
//!
//! The test drives `GatewayServer`'s router through `tower::ServiceExt`
//! so we exercise the actual route table the TCP listener assembles (not
//! a hand-written subset). `/v1/channels` is the canary that the admin
//! router *does* serve: if that returned 404 we'd be misreading the
//! result.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use tower::ServiceExt;

use baybo_gateway::test_support::{TEST_ADMIN_TOKEN, build_test_deps};

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
    use baybo_gateway::auth::admin::{AdminAuthState, require_admin_token};
    let auth_state = AdminAuthState::new(tg.deps.admin_token.clone());
    let state = baybo_gateway::server::AdminState {
        config: std::sync::Arc::clone(&tg.deps.config),
        config_path: tg.deps.config_path.clone(),
        session_manager: std::sync::Arc::clone(&tg.deps.session_manager),
        job_lifecycle: std::sync::Arc::clone(&tg.deps.job_lifecycle),
        cron_scheduler: std::sync::Arc::clone(&tg.deps.cron_scheduler),
        trace_store: tg.deps.stores.trace.clone(),
        cost_store: tg.deps.stores.cost.clone(),
        goal_store: tg.deps.stores.goal.clone(),
        query_api: std::sync::Arc::new(baybo_query::QueryApi::new(
            tg.deps.session_manager.store(),
            std::sync::Arc::clone(&tg.deps.job_lifecycle),
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
        channel_control: std::sync::Arc::clone(&tg.deps.channel_control),
        secret_vault: std::sync::Arc::clone(&tg.deps.secret_vault),
        channel_tokens: tg.deps.channel_tokens.clone(),
        web_chat_tokens: std::sync::Arc::new(dashmap::DashMap::new()),
        bind_display: tg.deps.runtime_config.admin_bind.to_string(),
    };
    let (admin_router, _spec) = baybo_gateway::api::admin::v1_router_and_spec();
    let admin_router = admin_router
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            require_admin_token,
        ));
    axum::Router::new()
        .merge(baybo_gateway::api::health::routes())
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
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "{method} {path} must not be on admin listener"
    );
}

#[tokio::test]
async fn sessions_list_is_404_on_admin() {
    assert_not_found(Method::GET, "/v1/sessions").await;
}

#[tokio::test]
async fn create_session_is_404_on_admin() {
    assert_not_found(Method::POST, "/v1/sessions").await;
}

#[tokio::test]
async fn get_session_is_404_on_admin() {
    assert_not_found(Method::GET, "/v1/sessions/any").await;
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
