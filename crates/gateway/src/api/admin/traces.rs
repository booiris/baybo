//! `/v1/traces/{session_id}` — session trace export.

use axum::Json;
use axum::extract::{Path, State};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::api::dto::ErrorBody;
use crate::server::AdminState;
use crate::{GatewayError, Result};

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new().routes(routes!(get_trace))
}

#[utoipa::path(
    get,
    path = "/traces/{session_id}",
    tag = "traces",
    params(
        ("session_id" = String, Path, description = "Session id whose trace to export"),
    ),
    responses(
        (
            status = 200,
            description = "Raw trace tree for the session. Shape mirrors `aura_trace::TraceNode` but is emitted as untyped JSON to keep the admin surface decoupled from internal trace crate changes.",
            body = serde_json::Value,
            content_type = "application/json",
        ),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "No trace for that session", body = ErrorBody),
    )
)]
async fn get_trace(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let trace = state
        .trace_store
        .load_trace(&session_id)
        .await
        .map_err(|e| GatewayError::Trace(e.to_string()))?
        .ok_or_else(|| GatewayError::NotFound(format!("trace {session_id}")))?;
    let value = serde_json::to_value(&trace).map_err(|e| GatewayError::Internal(e.to_string()))?;
    Ok(Json(value))
}
