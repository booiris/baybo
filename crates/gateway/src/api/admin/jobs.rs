//! `/v1/jobs` endpoints.

use axum::Json;
use axum::extract::{Path, State};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::api::dto::{ErrorBody, Job, ListResponse};
use crate::server::AdminState;
use crate::{GatewayError, Result};

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(list_jobs))
        .routes(routes!(get_job))
        .routes(routes!(cancel_job))
}

#[utoipa::path(
    get,
    path = "/jobs",
    tag = "jobs",
    responses(
        (status = 200, description = "All jobs currently tracked", body = inline(ListResponse<Job>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Job store error", body = ErrorBody),
    )
)]
async fn list_jobs(State(state): State<AdminState>) -> Result<Json<ListResponse<Job>>> {
    let items = state
        .job_manager
        .list(None)
        .await
        .map_err(|e| GatewayError::Job(e.to_string()))?
        .into_iter()
        .map(Job::from)
        .collect();
    Ok(Json(ListResponse::new(items)))
}

#[utoipa::path(
    get,
    path = "/jobs/{id}",
    tag = "jobs",
    params(
        ("id" = String, Path, description = "Job id"),
    ),
    responses(
        (status = 200, description = "Job record", body = Job),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
async fn get_job(State(state): State<AdminState>, Path(id): Path<String>) -> Result<Json<Job>> {
    let job = state
        .job_manager
        .get(&id)
        .await
        .map_err(|e| GatewayError::Job(e.to_string()))?
        .ok_or_else(|| GatewayError::NotFound(format!("job {id}")))?;
    Ok(Json(Job::from(job)))
}

#[utoipa::path(
    post,
    path = "/jobs/{id}/cancel",
    tag = "jobs",
    params(
        ("id" = String, Path, description = "Job id"),
    ),
    responses(
        (status = 200, description = "Cancelled job record", body = Job),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Cancel failed", body = ErrorBody),
    )
)]
async fn cancel_job(State(state): State<AdminState>, Path(id): Path<String>) -> Result<Json<Job>> {
    let job = state
        .job_manager
        .cancel(&id)
        .await
        .map_err(|e| GatewayError::Job(e.to_string()))?;
    Ok(Json(Job::from(job)))
}
