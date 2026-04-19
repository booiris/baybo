//! `/v1/jobs` endpoints.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::routing::{get, post};

use aura_job::Job;

use crate::api::dto::ListResponse;
use crate::server::AdminState;
use crate::{GatewayError, Result};

pub fn routes() -> Router<AdminState> {
    Router::new()
        .route("/jobs", get(list_jobs))
        .route("/jobs/{id}", get(get_job))
        .route("/jobs/{id}/cancel", post(cancel_job))
}

async fn list_jobs(State(state): State<AdminState>) -> Result<Json<ListResponse<Job>>> {
    let items = state
        .job_manager
        .list(None)
        .await
        .map_err(|e| GatewayError::Job(e.to_string()))?;
    Ok(Json(ListResponse::new(items)))
}

async fn get_job(State(state): State<AdminState>, Path(id): Path<String>) -> Result<Json<Job>> {
    let job = state
        .job_manager
        .get(&id)
        .await
        .map_err(|e| GatewayError::Job(e.to_string()))?
        .ok_or_else(|| GatewayError::NotFound(format!("job {id}")))?;
    Ok(Json(job))
}

async fn cancel_job(State(state): State<AdminState>, Path(id): Path<String>) -> Result<Json<Job>> {
    let job = state
        .job_manager
        .cancel(&id)
        .await
        .map_err(|e| GatewayError::Job(e.to_string()))?;
    Ok(Json(job))
}
