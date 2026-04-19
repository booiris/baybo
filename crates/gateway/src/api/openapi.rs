//! `/v1/openapi.json` — serve the admin API spec generated at startup.
//!
//! The same `OpenApi` document is exported to disk by the spec-drift
//! test (`tests/openapi_spec_sync.rs`) so `docs/openapi.json` always
//! matches what the running gateway returns.
//!
//! Deliberately **not** annotated with `#[utoipa::path]`: the spec
//! should not describe itself, and adding it as an API surface item
//! just clutters generated clients.

use std::sync::Arc;

use axum::Json;
use axum::routing::get;
use utoipa::openapi::OpenApi;

use crate::server::AdminState;

/// Build the `GET /v1/openapi.json` route. Typed over [`AdminState`]
/// so it merges cleanly into the admin router without a state-type
/// mismatch, though the handler itself ignores state.
pub fn spec_route(spec: OpenApi) -> axum::Router<AdminState> {
    let shared = Arc::new(spec);
    axum::Router::new().route(
        "/v1/openapi.json",
        get(move || {
            let shared = shared.clone();
            async move { Json((*shared).clone()) }
        }),
    )
}
