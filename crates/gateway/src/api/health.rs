//! Unauthenticated health and readiness probes.
//!
//! Mounted at the router root (outside `/v1`) and explicitly skipped by
//! the auth middleware so orchestrators can probe the gateway before a
//! token exists.

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;

pub fn routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
}

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz() -> (StatusCode, &'static str) {
    // The presence of this handler plus a successful auth state
    // implies the runtime finished assembly; richer checks (DB ping,
    // vault roundtrip) can land here later.
    (StatusCode::OK, "ok")
}
