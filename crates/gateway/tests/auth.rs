//! Crate-level tests for the `require_token` axum middleware.
//!
//! The middleware is unit-tested at the helper level in `src/auth.rs`;
//! these tests drive a real axum `Router` through `tower::ServiceExt`
//! so we cover what the framework actually sees: header vs. query
//! extraction, 401 paths, and the URI-sanitization contract that stops
//! tokens leaking into structured logs.

use aura_gateway::auth::{AuthState, require_token};
use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode, header};
use axum::middleware;
use axum::routing::get;
use tower::ServiceExt;

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Echo the `uri` seen at handler time as the response body so callers
/// can assert on middleware-side URI sanitization.
async fn echo_uri(req: Request) -> String {
    req.uri().to_string()
}

fn app() -> Router {
    let state = AuthState::new(TOKEN.to_owned());
    Router::new()
        .route("/v1/ping", get(echo_uri))
        .layer(middleware::from_fn_with_state(state, require_token))
}

fn req(method: Method, uri: &str, token_header: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(tok) = token_header {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {tok}"));
    }
    builder.body(Body::empty()).expect("request")
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
