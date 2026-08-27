//! `GET /v1/status` — snapshot of running gateway state.

use axum::extract::State;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::Result;
use crate::api::dto::{ErrorBody, StatusResponse};
use crate::server::AdminState;

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new().routes(routes!(status))
}

#[utoipa::path(
    get,
    path = "/status",
    tag = "status",
    responses(
        (status = 200, description = "Gateway status snapshot", body = StatusResponse),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 500, description = "Internal error", body = ErrorBody),
    )
)]
async fn status(State(state): State<AdminState>) -> Result<axum::Json<StatusResponse>> {
    // Both figures are SQL COUNTs — the probe never materialises the
    // session or turn rows it is counting.
    let sessions = state
        .session_manager
        .session_count()
        .await
        .map_err(|e| crate::GatewayError::Session(e.to_string()))?;
    let turns_in_flight = state
        .turn_lifecycle
        .count_by_status(baybo_turn::TurnStatusKind::InProgress)
        .await
        .map_err(|e: baybo_turn::TurnError| crate::GatewayError::Turn(e.to_string()))?;
    let server_key = crate::device::load_or_create_static_keypair(&state.secret_vault)
        .await
        .map_err(|e| crate::GatewayError::Internal(e.to_string()))?;
    Ok(axum::Json(StatusResponse {
        server_key: hex::encode(server_key.public()),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        bind_address: state.bind_display.clone(),
        sessions,
        turns_in_flight,
    }))
}
