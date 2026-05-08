//! `/v1/logs` integration test.
//!
//! Seeds the shared `LogBuffer` with synthetic records (bypassing the
//! tracing dispatcher so this stays thread-safe) and then drives the
//! admin router through `tower::ServiceExt` to confirm filter /
//! pagination params reach `LogBuffer::query` and the envelope shape
//! the webui expects comes back intact.

use std::sync::Arc;

use axum::body::{self, Body};
use axum::http::{Request, StatusCode, header};
use chrono::{Duration, Utc};
use serde::Deserialize;
use tower::ServiceExt;

use aura_gateway::log_buffer::LogLevel;
use aura_gateway::test_support::{TEST_ADMIN_TOKEN, build_test_deps};

fn auth(req: Request<Body>) -> Request<Body> {
    let (mut parts, body) = req.into_parts();
    parts.headers.insert(
        header::AUTHORIZATION,
        format!("Bearer {TEST_ADMIN_TOKEN}").parse().unwrap(),
    );
    Request::from_parts(parts, body)
}

#[derive(Debug, Deserialize)]
struct LogField {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    value: String,
}

#[derive(Debug, Deserialize)]
struct LogEntry {
    #[allow(dead_code)]
    id: u64,
    level: String,
    target: String,
    message: String,
    #[allow(dead_code)]
    fields: Vec<LogField>,
}

#[derive(Debug, Deserialize)]
struct LogsResponse {
    items: Vec<LogEntry>,
    total: usize,
}

async fn read_json(resp: axum::response::Response) -> LogsResponse {
    let status = resp.status();
    let bytes = body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("collect body");
    assert_eq!(
        status,
        StatusCode::OK,
        "body={}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).expect("LogsResponse")
}

async fn router_with_seed() -> (axum::Router, Arc<aura_gateway::log_buffer::LogBuffer>) {
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let buf = Arc::clone(&tg.deps.log_buffer);
    let now = Utc::now();
    buf.push_for_test(
        now - Duration::seconds(30),
        LogLevel::Info,
        "auth",
        "login ok",
    );
    buf.push_for_test(
        now - Duration::seconds(20),
        LogLevel::Error,
        "auth",
        "Failed to connect to Redis cache at 10.0.4.22:6379",
    );
    buf.push_for_test(
        now - Duration::seconds(10),
        LogLevel::Warn,
        "db",
        "slow query",
    );

    use aura_gateway::auth::admin::{AdminAuthState, require_admin_token};
    let auth_state = AdminAuthState::new(tg.deps.admin_token.clone());
    let state = aura_gateway::server::AdminState {
        config: Arc::clone(&tg.deps.config),
        config_path: tg.deps.config_path.clone(),
        session_manager: Arc::clone(&tg.deps.session_manager),
        job_lifecycle: Arc::clone(&tg.deps.job_lifecycle),
        cron_scheduler: Arc::clone(&tg.deps.cron_scheduler),
        memory_manager: Arc::clone(&tg.deps.memory_manager),
        trace_store: tg.deps.stores.trace.clone(),
        cost_store: tg.deps.stores.cost.clone(),
        query_api: Arc::new(aura_agent::QueryApi::new(
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
        bind_display: tg.deps.runtime_config.admin_bind.to_string(),
    };
    let (admin_router, _spec) = aura_gateway::api::admin::v1_router_and_spec();
    let admin_router = admin_router
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            require_admin_token,
        ));
    let router = axum::Router::new()
        .merge(aura_gateway::api::health::routes())
        .merge(admin_router);
    (router, buf)
}

#[tokio::test]
async fn list_logs_returns_all_newest_first() {
    let (router, _) = router_with_seed().await;
    let req = auth(
        Request::builder()
            .method("GET")
            .uri("/v1/logs")
            .body(Body::empty())
            .unwrap(),
    );
    let resp = router.oneshot(req).await.expect("oneshot");
    let body = read_json(resp).await;
    assert_eq!(body.total, 3);
    let messages: Vec<&str> = body.items.iter().map(|e| e.message.as_str()).collect();
    assert_eq!(
        messages,
        vec![
            "slow query",
            "Failed to connect to Redis cache at 10.0.4.22:6379",
            "login ok"
        ]
    );
}

#[tokio::test]
async fn list_logs_filters_by_level() {
    let (router, _) = router_with_seed().await;
    let req = auth(
        Request::builder()
            .method("GET")
            .uri("/v1/logs?level=error")
            .body(Body::empty())
            .unwrap(),
    );
    let resp = router.oneshot(req).await.expect("oneshot");
    let body = read_json(resp).await;
    assert_eq!(body.total, 1);
    assert_eq!(body.items[0].level, "error");
    assert_eq!(body.items[0].target, "auth");
}

#[tokio::test]
async fn list_logs_filters_by_needle_across_message_and_target() {
    let (router, _) = router_with_seed().await;
    let req = auth(
        Request::builder()
            .method("GET")
            .uri("/v1/logs?q=db")
            .body(Body::empty())
            .unwrap(),
    );
    let resp = router.oneshot(req).await.expect("oneshot");
    let body = read_json(resp).await;
    assert_eq!(body.total, 1);
    assert_eq!(body.items[0].target, "db");
}

#[tokio::test]
async fn list_logs_paginates() {
    let (router, _) = router_with_seed().await;
    let req = auth(
        Request::builder()
            .method("GET")
            .uri("/v1/logs?limit=1&offset=1")
            .body(Body::empty())
            .unwrap(),
    );
    let resp = router.oneshot(req).await.expect("oneshot");
    let body = read_json(resp).await;
    assert_eq!(body.total, 3, "total ignores limit/offset");
    assert_eq!(body.items.len(), 1);
    assert_eq!(
        body.items[0].message,
        "Failed to connect to Redis cache at 10.0.4.22:6379"
    );
}

#[tokio::test]
async fn stream_logs_pushes_matching_records_as_sse() {
    use futures::StreamExt;
    use http_body_util::BodyStream;

    let (router, buf) = router_with_seed().await;
    let req = auth(
        Request::builder()
            .method("GET")
            .uri("/v1/logs/stream?level=error")
            .body(Body::empty())
            .unwrap(),
    );
    let resp = router.oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream"),
    );

    // Drive the buffer after the subscriber is wired so the broadcast
    // has a live receiver. One non-matching info and one matching error.
    let now = Utc::now();
    buf.push_for_test(now, LogLevel::Info, "auth", "ignore me");
    buf.push_for_test(now, LogLevel::Error, "auth", "boom");

    let mut body = BodyStream::new(resp.into_body());
    let mut buffered = Vec::<u8>::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut got_log = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();
        let frame = match tokio::time::timeout(remaining, body.next()).await {
            Ok(Some(Ok(frame))) => frame,
            _ => break,
        };
        if let Ok(data) = frame.into_data() {
            buffered.extend_from_slice(&data);
        }
        let text = String::from_utf8_lossy(&buffered);
        if text.contains("event: log") && text.contains("\"message\":\"boom\"") {
            got_log = true;
            break;
        }
    }
    assert!(
        got_log,
        "stream never emitted matching log event. Buffered: {}",
        String::from_utf8_lossy(&buffered)
    );
    assert!(
        !String::from_utf8_lossy(&buffered).contains("ignore me"),
        "stream leaked a non-matching record",
    );
}

#[tokio::test]
async fn list_logs_requires_bearer_token() {
    let (router, _) = router_with_seed().await;
    let req = Request::builder()
        .method("GET")
        .uri("/v1/logs")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
