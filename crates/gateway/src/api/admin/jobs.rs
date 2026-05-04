//! `/v1/jobs` endpoints.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use utoipa::IntoParams;
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

/// Default page size when `?limit=` is omitted. Picked to be small
/// enough that an unfiltered list on a large workspace doesn't page
/// the operator's terminal in one go, but large enough for normal
/// dashboards.
const DEFAULT_PAGE_LIMIT: usize = 50;
/// Hard ceiling so a malicious / mistyped `?limit=` can't allocate
/// arbitrary memory.
const MAX_PAGE_LIMIT: usize = 500;

#[derive(Debug, Deserialize, IntoParams)]
struct ListJobsParams {
    /// Restrict to a single session. Hits the per-session index in
    /// the store instead of scanning the full jobs table.
    session: Option<String>,
    /// Restrict to one terminal/in-flight status discriminator.
    /// Snake-case, matching `JobStatusKind` (`pending`, `in_progress`,
    /// `stuck`, `cancelled`, `failed`, `completed`).
    status: Option<String>,
    /// Maximum items to return. Defaults to 50; capped at 500.
    limit: Option<usize>,
    /// Opaque cursor from a previous response's `next_cursor`.
    cursor: Option<String>,
}

fn parse_status(s: &str) -> Result<aura_job::JobStatusKind> {
    use aura_job::JobStatusKind;
    match s {
        "pending" => Ok(JobStatusKind::Pending),
        "in_progress" => Ok(JobStatusKind::InProgress),
        "stuck" => Ok(JobStatusKind::Stuck),
        "cancelled" => Ok(JobStatusKind::Cancelled),
        "failed" => Ok(JobStatusKind::Failed),
        "completed" => Ok(JobStatusKind::Completed),
        other => Err(GatewayError::Job(format!(
            "invalid status filter: {other:?}"
        ))),
    }
}

#[utoipa::path(
    get,
    path = "/jobs",
    tag = "jobs",
    params(ListJobsParams),
    responses(
        (status = 200, description = "Paginated jobs", body = inline(ListResponse<Job>)),
        (status = 400, description = "Invalid query parameters", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Job store error", body = ErrorBody),
    )
)]
async fn list_jobs(
    State(state): State<AdminState>,
    Query(params): Query<ListJobsParams>,
) -> Result<Json<ListResponse<Job>>> {
    let status = params.status.as_deref().map(parse_status).transpose()?;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_PAGE_LIMIT)
        .clamp(1, MAX_PAGE_LIMIT);
    let offset: usize = match params.cursor.as_deref() {
        Some(c) => c
            .parse()
            .map_err(|_| GatewayError::Job(format!("invalid cursor: {c:?}")))?,
        None => 0,
    };

    let mut jobs = match params.session {
        Some(sid) => {
            let sid = aura_model::SessionId::from(sid);
            state.job_lifecycle.list_by_session(&sid, status).await
        }
        None => state.job_lifecycle.list(status).await,
    }
    .map_err(|e: aura_job::JobError| GatewayError::Job(e.to_string()))?;

    let total = jobs.len();
    if offset >= total {
        return Ok(Json(ListResponse::new(Vec::new())));
    }
    let page: Vec<_> = jobs.drain(offset..).take(limit).map(Job::from).collect();
    let next_cursor = if offset + page.len() < total {
        Some((offset + page.len()).to_string())
    } else {
        None
    };
    Ok(Json(ListResponse::with_next_cursor(page, next_cursor)))
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
    let job_id: aura_model::JobId = id
        .parse()
        .map_err(|e| GatewayError::Job(format!("invalid job id: {e}")))?;
    let job = state
        .job_lifecycle
        .get(&job_id)
        .await
        .map_err(|e: aura_job::JobError| GatewayError::Job(e.to_string()))?
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
    let job_id: aura_model::JobId = id
        .parse()
        .map_err(|e| GatewayError::Job(format!("invalid job id: {e}")))?;
    // Operator-initiated cancel; partial-artifact rollup belongs on the
    // recovery scan, not the admin endpoint.
    state
        .job_lifecycle
        .cancel(&job_id, aura_job::CancelReason::HookAborted, vec![])
        .await
        .map_err(|e: aura_job::JobError| GatewayError::Job(e.to_string()))?;
    let job = state
        .job_lifecycle
        .get(&job_id)
        .await
        .map_err(|e: aura_job::JobError| GatewayError::Job(e.to_string()))?
        .ok_or_else(|| GatewayError::NotFound(format!("job {id}")))?;
    Ok(Json(Job::from(job)))
}
