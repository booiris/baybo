//! `/v1/jobs` endpoints.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use utoipa::IntoParams;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::api::dto::{BackgroundJob, BackgroundJobsResponse, ErrorBody, Job, ListResponse};
use crate::server::AdminState;
use crate::{GatewayError, Result};

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(list_jobs))
        .routes(routes!(get_job))
        .routes(routes!(cancel_job))
        .routes(routes!(list_background_jobs))
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

fn parse_status(s: &str) -> Result<baybo_job::JobStatusKind> {
    use baybo_job::JobStatusKind;
    match s {
        "pending" => Ok(JobStatusKind::Pending),
        "in_progress" => Ok(JobStatusKind::InProgress),
        "stuck" => Ok(JobStatusKind::Stuck),
        "cancelled" => Ok(JobStatusKind::Cancelled),
        "failed" => Ok(JobStatusKind::Failed),
        "completed" => Ok(JobStatusKind::Completed),
        other => Err(GatewayError::BadRequest(format!(
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
            .map_err(|_| GatewayError::BadRequest(format!("invalid cursor: {c:?}")))?,
        None => 0,
    };

    let (page_jobs, total) = match params.session {
        // Session-scoped lists stay a per-session load (bounded by that
        // session's job count) with in-memory paging.
        Some(sid) => {
            let sid = baybo_model::SessionId::from(sid);
            let mut jobs = state
                .job_lifecycle
                .list_by_session(&sid, status)
                .await
                .map_err(|e: baybo_job::JobError| GatewayError::Job(e.to_string()))?;
            let total = jobs.len();
            if offset >= total {
                return Ok(Json(ListResponse::new(Vec::new())));
            }
            let page: Vec<_> = jobs.drain(offset..).take(limit).collect();
            (page, total)
        }
        // The unscoped list pages in SQL — a page never materialises
        // the whole jobs table.
        None => state
            .job_lifecycle
            .list_page(status, limit, offset)
            .await
            .map_err(|e: baybo_job::JobError| GatewayError::Job(e.to_string()))?,
    };
    let page: Vec<_> = page_jobs.into_iter().map(Job::from).collect();
    let next_cursor = if offset + page.len() < total {
        Some((offset + page.len()).to_string())
    } else {
        None
    };
    Ok(Json(ListResponse::with_next_cursor(page, next_cursor)))
}

/// Cross-session view of in-flight background jobs (detached subagents +
/// `Bash` commands) — the dashboard twin of the per-session `JobList` tool.
#[utoipa::path(
    get,
    path = "/background-jobs",
    tag = "jobs",
    responses(
        (status = 200, description = "In-flight background jobs", body = BackgroundJobsResponse),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn list_background_jobs(State(state): State<AdminState>) -> Json<BackgroundJobsResponse> {
    let jobs = state
        .supervisor
        .list_all_in_flight_background()
        .into_iter()
        .map(|(parent, info)| BackgroundJob {
            handle: info.handle,
            session_id: parent.as_ref().to_string(),
            kind: info.kind,
            summary: info.task_summary,
        })
        .collect();
    Json(BackgroundJobsResponse { jobs })
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
        (status = 400, description = "Invalid job id", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
async fn get_job(State(state): State<AdminState>, Path(id): Path<String>) -> Result<Json<Job>> {
    let job_id: baybo_model::JobId = id
        .parse()
        .map_err(|e| GatewayError::BadRequest(format!("invalid job id: {e}")))?;
    let job = state
        .job_lifecycle
        .get(&job_id)
        .await
        .map_err(|e: baybo_job::JobError| GatewayError::Job(e.to_string()))?
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
        (status = 400, description = "Invalid job id", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 500, description = "Cancel failed", body = ErrorBody),
    )
)]
async fn cancel_job(State(state): State<AdminState>, Path(id): Path<String>) -> Result<Json<Job>> {
    let job_id: baybo_model::JobId = id
        .parse()
        .map_err(|e| GatewayError::BadRequest(format!("invalid job id: {e}")))?;
    // Operator-initiated cancel; partial-artifact rollup belongs on the
    // recovery scan, not the admin endpoint.
    state
        .job_lifecycle
        .cancel(&job_id, baybo_job::CancelReason::OperatorCancel, vec![])
        .await
        .map_err(|e: baybo_job::JobError| GatewayError::Job(e.to_string()))?;
    let job = state
        .job_lifecycle
        .get(&job_id)
        .await
        .map_err(|e: baybo_job::JobError| GatewayError::Job(e.to_string()))?
        .ok_or_else(|| GatewayError::NotFound(format!("job {id}")))?;
    Ok(Json(Job::from(job)))
}
