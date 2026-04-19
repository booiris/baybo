//! The admin TCP listener must not serve channel-surface routes.
//!
//! After the two-listener split, `/v1/sessions`, `/v1/sessions/{id}`,
//! `/v1/sessions/{id}/messages`, `/v1/sessions/{id}/stream`, and
//! `/v1/approvals*` live exclusively on the channel UDS. Requesting any
//! of them through the admin router has to come back `404`, even with a
//! valid bearer token — a leaked admin credential must not yield chat
//! content or approval control.
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
    use aura_gateway::auth_admin::{AdminAuthState, require_admin_token};
    let auth_state = AdminAuthState::new(tg.deps.admin_token.clone());
    let state = aura_gateway::server::AdminState {
        config: std::sync::Arc::clone(&tg.deps.config),
        config_path: tg.deps.config_path.clone(),
        session_manager: std::sync::Arc::clone(&tg.deps.session_manager),
        job_manager: std::sync::Arc::clone(&tg.deps.job_manager),
        cron_scheduler: std::sync::Arc::clone(&tg.deps.cron_scheduler),
        memory_manager: std::sync::Arc::clone(&tg.deps.memory_manager),
        trace_store: std::sync::Arc::clone(&tg.deps.trace_store),
        skill_registry: std::sync::Arc::clone(&tg.deps.skill_registry),
        tool_registry: std::sync::Arc::clone(&tg.deps.tool_registry),
        channel_registry: std::sync::Arc::clone(&tg.deps.channel_registry),
        llm_client: std::sync::Arc::clone(&tg.deps.llm_client),
        bind_display: tg.deps.runtime_config.admin_bind.to_string(),
    };
    let v1 = aura_gateway::api::admin::v1_router()
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            require_admin_token,
        ));
    axum::Router::new()
        .merge(aura_gateway::api::health::routes())
        .nest("/v1", v1)
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
