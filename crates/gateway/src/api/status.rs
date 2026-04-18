//! `GET /v1/status` — snapshot of running gateway state.

use axum::Router;
use axum::extract::State;
use axum::routing::get;
use serde::Serialize;

use crate::Result;
use crate::server::ApiState;

pub fn routes() -> Router<ApiState> {
    Router::new().route("/status", get(status))
}

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub version: &'static str,
    pub bind_address: String,
    pub sessions: usize,
    pub jobs_in_flight: usize,
}

async fn status(State(state): State<ApiState>) -> Result<axum::Json<StatusResponse>> {
    let sessions = state
        .session_manager
        .list()
        .await
        .map_err(|e| crate::GatewayError::Session(e.to_string()))?
        .len();
    let jobs_in_flight = state
        .job_manager
        .list(Some(aura_job::JobStatus::InProgress))
        .await
        .map_err(|e| crate::GatewayError::Job(e.to_string()))?
        .len();
    Ok(axum::Json(StatusResponse {
        version: env!("CARGO_PKG_VERSION"),
        bind_address: state.bind_display.clone(),
        sessions,
        jobs_in_flight,
    }))
}
