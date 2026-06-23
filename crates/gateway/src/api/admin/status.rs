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
    let sessions = state
        .session_manager
        .list()
        .await
        .map_err(|e| crate::GatewayError::Session(e.to_string()))?
        .len();
    let jobs_in_flight = state
        .job_lifecycle
        .list(Some(baybo_job::JobStatusKind::InProgress))
        .await
        .map_err(|e: baybo_job::JobError| crate::GatewayError::Job(e.to_string()))?
        .len();
    Ok(axum::Json(StatusResponse {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        bind_address: state.bind_display.clone(),
        sessions,
        jobs_in_flight,
    }))
}
