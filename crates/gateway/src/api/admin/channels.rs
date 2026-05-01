//! `/v1/channels` — read-only list of registered channels and their
//! statuses. Lives on the admin listener so operators can see
//! channel registrations; messaging/session routes live on the
//! loopback-TCP channel listener.

use std::sync::Arc;

use aura_channels::ChannelRegistry;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::Result;
use crate::api::dto::{ChannelEntry, ErrorBody, ListResponse};
use crate::channel::diagnose::{DiagnoseError, request_diagnose};
use crate::server::AdminState;

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(list_channels))
        .routes(routes!(diagnose_channel))
}

#[utoipa::path(
    get,
    path = "/channels",
    tag = "channels",
    responses(
        (status = 200, description = "Registered channels", body = inline(ListResponse<ChannelEntry>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn list_channels(
    State(state): State<AdminState>,
) -> Result<Json<ListResponse<ChannelEntry>>> {
    let items = snapshot(&state.channel_registry);
    Ok(Json(ListResponse::new(items)))
}

fn snapshot(registry: &Arc<ChannelRegistry>) -> Vec<ChannelEntry> {
    registry
        .list()
        .into_iter()
        .map(|ct| ChannelEntry {
            channel_type: ct.into(),
            status: "running".to_string(),
        })
        .collect()
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct DiagnoseQuery {
    pub bot_id: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DiagnoseResponse {
    pub channel_type: String,
    pub bot_id: String,
    pub checks: Vec<DiagnoseCheckEntry>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DiagnoseCheckEntry {
    pub name: String,
    /// `"ok"` / `"warn"` / `"error"` — string-typed so third-party
    /// clients can render without a typed enum.
    pub status: String,
    pub detail: String,
}

/// Round-trip a diagnose self-test against the sidecar serving
/// `:channel_type`. The request is gated by the sidecar's advertised
/// `"diagnose"` capability — sidecars that don't claim it return
/// HTTP 501 instead of timing out.
#[utoipa::path(
    get,
    path = "/channels/{channel_type}/diagnose",
    tag = "channels",
    params(
        ("channel_type" = String, Path, description = "Sidecar channel type (e.g. `lark`)"),
        ("bot_id" = String, Query, description = "Per-tenant bot id"),
    ),
    responses(
        (status = 200, description = "Diagnose report", body = inline(DiagnoseResponse)),
        (status = 404, description = "No sidecar connected", body = ErrorBody),
        (status = 501, description = "Sidecar lacks `diagnose` capability", body = ErrorBody),
        (status = 504, description = "Sidecar did not reply in time", body = ErrorBody),
    )
)]
async fn diagnose_channel(
    State(state): State<AdminState>,
    Path(channel_type): Path<String>,
    Query(query): Query<DiagnoseQuery>,
) -> std::result::Result<Json<DiagnoseResponse>, (StatusCode, Json<ErrorBody>)> {
    let ct = aura_model::ChannelType::from(channel_type.as_str());
    let cap_advertised = state
        .channel_capabilities
        .supports(&ct, crate::channel::handshake::CAP_DIAGNOSE);
    let report = request_diagnose(
        &state.channel_control,
        &state.diagnose_router,
        cap_advertised,
        &ct,
        query.bot_id,
    )
    .await
    .map_err(map_diagnose_error)?;
    Ok(Json(DiagnoseResponse {
        channel_type: ct.to_string(),
        bot_id: report.bot_id,
        checks: report
            .checks
            .into_iter()
            .map(|c| DiagnoseCheckEntry {
                name: c.name,
                status: status_as_str(c.status).to_string(),
                detail: c.detail,
            })
            .collect(),
    }))
}

fn status_as_str(status: aura_channels::wire::DiagnoseStatus) -> &'static str {
    match status {
        aura_channels::wire::DiagnoseStatus::Ok => "ok",
        aura_channels::wire::DiagnoseStatus::Warn => "warn",
        aura_channels::wire::DiagnoseStatus::Error => "error",
    }
}

fn map_diagnose_error(err: DiagnoseError) -> (StatusCode, Json<ErrorBody>) {
    let (code, message) = match &err {
        DiagnoseError::NotConnected(_) => (StatusCode::NOT_FOUND, err.to_string()),
        DiagnoseError::CapabilityMissing(_) => (StatusCode::NOT_IMPLEMENTED, err.to_string()),
        DiagnoseError::Disconnected => (StatusCode::SERVICE_UNAVAILABLE, err.to_string()),
        DiagnoseError::Timeout(_) => (StatusCode::GATEWAY_TIMEOUT, err.to_string()),
        DiagnoseError::SidecarError(_) | DiagnoseError::Control(_) => {
            (StatusCode::BAD_GATEWAY, err.to_string())
        }
    };
    (code, Json(ErrorBody { error: message }))
}
