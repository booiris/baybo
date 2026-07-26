//! Crate-level tests for the admin `require_admin_token` axum middleware.
//!
//! The middleware is unit-tested at the helper level in `src/auth/admin.rs`;
//! these tests drive a real axum `Router` through `tower::ServiceExt`
//! so we cover what the framework actually sees: header vs. query
//! extraction, 401 paths, and the URI-sanitization contract that stops
//! tokens leaking into structured logs.

use axum::Router;
use axum::body::Body;
use axum::extract::Extension;
use axum::extract::Request;
use axum::http::{Method, StatusCode, header};
use axum::middleware;
use axum::routing::get;
use baybo_gateway::auth::admin::{AdminAuthState, require_admin_token};
use baybo_gateway::auth::{AuthedClient, DEVICE_ID_HEADER};
use baybo_storage::test_support::MemoryDeviceStore;
use baybo_store::device::hash_auth_token;
use baybo_store::{DeviceRow, DeviceStatus, DeviceStore};
use std::sync::Arc;
use tower::ServiceExt;

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Echo the `uri` seen at handler time as the response body so callers
/// can assert on middleware-side URI sanitization.
async fn echo_uri(req: Request) -> String {
    req.uri().to_string()
}

async fn echo_identity(Extension(authed): Extension<AuthedClient>) -> &'static str {
    match authed {
        AuthedClient::Web => "web",
        AuthedClient::Device { .. } => "device",
        AuthedClient::Tui | AuthedClient::Tool { .. } | AuthedClient::Subprocess { .. } => "other",
    }
}

fn app_with_state(state: AdminAuthState) -> Router {
    Router::new()
        .route("/v1/ping", get(echo_uri))
        .route("/v1/who", get(echo_identity))
        .layer(middleware::from_fn_with_state(state, require_admin_token))
}

fn app() -> Router {
    app_with_state(AdminAuthState::new(TOKEN.to_owned()))
}

fn req(method: Method, uri: &str, token_header: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(tok) = token_header {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {tok}"));
    }
    builder.body(Body::empty()).expect("request")
}

fn device_req(uri: &str, token: &str, device_id: &str) -> Request<Body> {
    Request::builder()
        .method(Method::GET)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(DEVICE_ID_HEADER, device_id)
        .body(Body::empty())
        .expect("request")
}

fn device_id() -> String {
    let key = device_proto::delegation::generate_signing_key();
    device_proto::delegation::device_id_for(&key.verifying_key())
}

fn approved_device(device_id: &str, auth_token: &str) -> DeviceRow {
    DeviceRow {
        device_id: device_id.into(),
        device_pubkey: vec![0u8; 32],
        auth_token_sha256: hash_auth_token(auth_token),
        status: DeviceStatus::Approved,
        rendezvous_id: Some("11111111-2222-4333-8444-555555555555".into()),
        created_at: 1,
        approved_at: Some(2),
        last_seen_at: None,
        relay_url: "wss://relay.test".into(),
        remote_api_key: "inst-test".into(),
    }
}

#[tokio::test]
async fn missing_token_is_unauthorized() {
    let resp = app()
        .oneshot(req(Method::GET, "/v1/ping", None))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_token_is_unauthorized() {
    let resp = app()
        .oneshot(req(Method::GET, "/v1/ping", Some("deadbeef")))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bearer_header_accepted() {
    let resp = app()
        .oneshot(req(Method::GET, "/v1/ping", Some(TOKEN)))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn query_token_accepted() {
    let uri = format!("/v1/ping?token={TOKEN}");
    let resp = app()
        .oneshot(req(Method::GET, &uri, None))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn query_token_stripped_before_handler() {
    // After the middleware runs, the handler (and any tracing layer
    // downstream of it) must not see `token=...` in the URI.
    let uri = format!("/v1/ping?token={TOKEN}&x=1");
    let resp = app()
        .oneshot(req(Method::GET, &uri, None))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), 1024)
        .await
        .expect("body");
    let seen = std::str::from_utf8(&body).expect("utf8");
    assert!(
        !seen.contains("token="),
        "handler saw unsanitised URI: {seen}"
    );
    assert!(seen.contains("x=1"), "non-token query dropped: {seen}");
}

#[tokio::test]
async fn query_token_sole_param_is_stripped() {
    let uri = format!("/v1/ping?token={TOKEN}");
    let resp = app()
        .oneshot(req(Method::GET, &uri, None))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), 1024)
        .await
        .expect("body");
    let seen = std::str::from_utf8(&body).expect("utf8");
    assert!(!seen.contains("token="), "sole token not stripped: {seen}");
    assert!(!seen.contains('?'), "empty query left trailing `?`: {seen}");
}

#[tokio::test]
async fn header_beats_query_on_mismatch() {
    // Header takes precedence; a correct header with a bogus query
    // token must still authenticate.
    let uri = "/v1/ping?token=wrong";
    let resp = app()
        .oneshot(req(Method::GET, uri, Some(TOKEN)))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn admin_token_with_device_header_marks_device() {
    let id = device_id();
    let resp = app()
        .oneshot(device_req("/v1/who", TOKEN, &id))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), 1024)
        .await
        .expect("body");
    assert_eq!(std::str::from_utf8(&body).expect("utf8"), "device");
}

#[tokio::test]
async fn device_token_with_matching_device_header_marks_device() {
    let id = device_id();
    let store = Arc::new(MemoryDeviceStore::new());
    store
        .create(&approved_device(&id, "device-token"))
        .await
        .expect("seed device");
    let state = AdminAuthState::new(TOKEN.to_owned()).with_device_store(store);

    let resp = app_with_state(state)
        .oneshot(device_req("/v1/who", "device-token", &id))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), 1024)
        .await
        .expect("body");
    assert_eq!(std::str::from_utf8(&body).expect("utf8"), "device");
}

#[tokio::test]
async fn device_token_with_mismatched_device_header_is_unauthorized() {
    let id = device_id();
    let other = device_id();
    let store = Arc::new(MemoryDeviceStore::new());
    store
        .create(&approved_device(&id, "device-token"))
        .await
        .expect("seed device");
    let state = AdminAuthState::new(TOKEN.to_owned()).with_device_store(store);

    let resp = app_with_state(state)
        .oneshot(device_req("/v1/who", "device-token", &other))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn device_token_without_device_header_is_unauthorized() {
    let id = device_id();
    let store = Arc::new(MemoryDeviceStore::new());
    store
        .create(&approved_device(&id, "device-token"))
        .await
        .expect("seed device");
    let state = AdminAuthState::new(TOKEN.to_owned()).with_device_store(store);

    let resp = app_with_state(state)
        .oneshot(req(Method::GET, "/v1/who", Some("device-token")))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn malformed_device_header_is_bad_request() {
    let resp = app()
        .oneshot(device_req("/v1/who", TOKEN, "device-1"))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
