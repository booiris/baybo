//! **dashboard** — the operator's read + control transport plane for
//! `remote-host`.
//!
//! Blind by construction: this crate is pure transport. It serves the operator
//! UI (three embedded text assets) and proxies typed calls to a
//! [`DashboardBackend`] the runtime implements over the live admission / relay
//! / push / traffic components. It never touches a domain crate, never carries
//! a relay payload (the relay moves Noise ciphertext) and never carries an APNs
//! token — the backend masks keys to their last four characters and devices to
//! a token tail before any DTO reaches the wire. The bearer token is presented
//! only as an `Authorization` header, so CSRF is structurally impossible.

mod auth;
mod types;

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, patch, post};
use axum::{Json, Router};

pub use auth::DashboardToken;
pub use types::{
    AdmitKeyOutcome, AdmitKeyRequest, BucketUnit, DashboardError, DeviceRow, EditKeyRequest,
    IpBucket, IpEndpointBreakdown, IpEndpointTotal, IpTotal, IpTotals, IpTrafficSeries, KeyRow,
    KickOutcome, OverviewSnapshot, PushBucket, PushDeviceTotal, PushTotals, PushTrafficSeries,
    Range, RangeQuery, RelayBucket, RelayKeyTotal, RelayTotals, RelayTrafficSeries, RevealedKey,
};

use auth::require_token;

/// Where the dashboard mounts on the shared public listener. Referenced wherever
/// a route path or the server-side mount is built.
pub const MOUNT_PREFIX: &str = "/dashboard";

const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_CSS: &str = include_str!("../assets/app.css");
const APP_JS: &str = include_str!("../assets/app.js");

const HTML_CT: &str = "text/html; charset=utf-8";
const CSS_CT: &str = "text/css; charset=utf-8";
const JS_CT: &str = "text/javascript; charset=utf-8";

/// The read + control surface the runtime implements over its live components.
/// Every method is `async` and `Result`-returning so the `dyn` trait stays
/// uniform whether a call hits sqlite or an in-memory registry.
#[async_trait::async_trait]
pub trait DashboardBackend: Send + Sync {
    async fn overview(&self) -> Result<OverviewSnapshot, DashboardError>;
    async fn list_keys(&self) -> Result<Vec<KeyRow>, DashboardError>;
    async fn reveal_key(&self, id: i64) -> Result<RevealedKey, DashboardError>;
    async fn admit_key(&self, req: AdmitKeyRequest) -> Result<AdmitKeyOutcome, DashboardError>;
    async fn edit_key(&self, id: i64, req: EditKeyRequest) -> Result<(), DashboardError>;
    async fn revoke_key(&self, id: i64) -> Result<(), DashboardError>;
    async fn kick_key(&self, id: i64) -> Result<KickOutcome, DashboardError>;
    async fn traffic_relay(&self, range: Range) -> Result<RelayTrafficSeries, DashboardError>;
    async fn traffic_push(&self, range: Range) -> Result<PushTrafficSeries, DashboardError>;
    async fn traffic_ip(&self, range: Range) -> Result<IpTrafficSeries, DashboardError>;
    async fn traffic_ip_endpoints(
        &self,
        ip: String,
        range: Range,
    ) -> Result<IpEndpointBreakdown, DashboardError>;
    async fn list_devices(&self) -> Result<Vec<DeviceRow>, DashboardError>;
}

#[derive(Clone)]
pub(crate) struct DashState {
    backend: Arc<dyn DashboardBackend>,
    token: DashboardToken,
}

/// Required dependencies for [`router`], populated by struct literal at the one
/// server call site.
pub struct DashboardConfig {
    pub backend: Arc<dyn DashboardBackend>,
    pub token: DashboardToken,
}

/// Build the dashboard router: open static-asset routes plus the
/// bearer-gated `/dashboard/api/*` subtree. Routes are absolute under
/// [`MOUNT_PREFIX`]; the server `merge`s this (never `nest`).
pub fn router(config: DashboardConfig) -> Router {
    let state = DashState {
        backend: config.backend,
        token: config.token,
    };
    let p = MOUNT_PREFIX;
    let api = Router::new()
        .route(&format!("{p}/api/overview"), get(overview))
        .route(&format!("{p}/api/keys"), get(list_keys).post(admit_key))
        .route(
            &format!("{p}/api/keys/{{id}}"),
            patch(edit_key).delete(revoke_key),
        )
        .route(&format!("{p}/api/keys/{{id}}/reveal"), get(reveal_key))
        .route(&format!("{p}/api/keys/{{id}}/kick"), post(kick_key))
        .route(&format!("{p}/api/traffic/relay"), get(traffic_relay))
        .route(&format!("{p}/api/traffic/push"), get(traffic_push))
        .route(&format!("{p}/api/traffic/ip"), get(traffic_ip))
        .route(
            &format!("{p}/api/traffic/ip/{{ip}}/endpoints"),
            get(traffic_ip_endpoints),
        )
        .route(&format!("{p}/api/devices"), get(list_devices))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token));
    Router::new()
        .route("/", get(|| async { Redirect::temporary(MOUNT_PREFIX) }))
        .route(p, get(index))
        .route(&format!("{p}/app.css"), get(app_css))
        .route(&format!("{p}/app.js"), get(app_js))
        .merge(api)
        .with_state(state)
}

async fn index() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, HTML_CT)], INDEX_HTML)
}

async fn app_css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, CSS_CT)], APP_CSS)
}

async fn app_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, JS_CT)], APP_JS)
}

async fn overview(State(state): State<DashState>) -> Result<impl IntoResponse, DashboardError> {
    Ok(Json(state.backend.overview().await?))
}

async fn list_keys(State(state): State<DashState>) -> Result<impl IntoResponse, DashboardError> {
    Ok(Json(state.backend.list_keys().await?))
}

async fn admit_key(
    State(state): State<DashState>,
    Json(req): Json<AdmitKeyRequest>,
) -> Result<impl IntoResponse, DashboardError> {
    let outcome = state.backend.admit_key(req).await?;
    Ok((StatusCode::CREATED, Json(outcome)))
}

async fn reveal_key(
    State(state): State<DashState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, DashboardError> {
    Ok(Json(state.backend.reveal_key(id).await?))
}

async fn edit_key(
    State(state): State<DashState>,
    Path(id): Path<i64>,
    Json(req): Json<EditKeyRequest>,
) -> Result<impl IntoResponse, DashboardError> {
    state.backend.edit_key(id, req).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn revoke_key(
    State(state): State<DashState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, DashboardError> {
    state.backend.revoke_key(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn kick_key(
    State(state): State<DashState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, DashboardError> {
    Ok(Json(state.backend.kick_key(id).await?))
}

async fn traffic_relay(
    State(state): State<DashState>,
    Query(q): Query<RangeQuery>,
) -> Result<impl IntoResponse, DashboardError> {
    Ok(Json(state.backend.traffic_relay(q.range).await?))
}

async fn traffic_push(
    State(state): State<DashState>,
    Query(q): Query<RangeQuery>,
) -> Result<impl IntoResponse, DashboardError> {
    Ok(Json(state.backend.traffic_push(q.range).await?))
}

async fn traffic_ip(
    State(state): State<DashState>,
    Query(q): Query<RangeQuery>,
) -> Result<impl IntoResponse, DashboardError> {
    Ok(Json(state.backend.traffic_ip(q.range).await?))
}

async fn traffic_ip_endpoints(
    State(state): State<DashState>,
    Path(ip): Path<String>,
    Query(q): Query<RangeQuery>,
) -> Result<impl IntoResponse, DashboardError> {
    Ok(Json(state.backend.traffic_ip_endpoints(ip, q.range).await?))
}

async fn list_devices(State(state): State<DashState>) -> Result<impl IntoResponse, DashboardError> {
    Ok(Json(state.backend.list_devices().await?))
}

#[cfg(test)]
mod tests;
