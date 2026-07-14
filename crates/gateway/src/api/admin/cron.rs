//! `/v1/cron` endpoints.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use utoipa::IntoParams;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use baybo_cron::{CronError, CronSchedule};
use baybo_model::ChannelType as ChannelTypeModel;

use crate::api::dto::{CreateCronRequest, CronJob, ErrorBody, ListResponse};
use crate::server::AdminState;
use crate::{GatewayError, Result};

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(list_cron, create_cron))
        .routes(routes!(get_cron, delete_cron))
        .routes(routes!(pause_cron))
        .routes(routes!(resume_cron))
        .routes(routes!(restore_cron))
}

/// Map a scheduler error onto the right HTTP status: an unknown job id is
/// 404, a schedule with no future fire time (resuming a one-shot whose
/// moment has passed) is 400, anything else is a 500.
fn cron_err(e: CronError) -> GatewayError {
    match e {
        CronError::NotFound(m) => GatewayError::NotFound(m),
        invalid @ CronError::InvalidSchedule(_) => GatewayError::BadRequest(invalid.to_string()),
        other => GatewayError::Cron(other.to_string()),
    }
}

/// Query string for `GET /v1/cron`.
#[derive(Debug, Default, Deserialize, IntoParams)]
pub struct ListCronQuery {
    /// Serve the recycle bin instead of the live list: the soft-deleted
    /// jobs, most recently deleted first. Defaults to false, and the
    /// default list never carries a deleted job.
    #[serde(default)]
    pub deleted: bool,
}

#[utoipa::path(
    get,
    path = "/cron",
    tag = "cron",
    params(ListCronQuery),
    responses(
        (status = 200, description = "Live cron jobs, or the recycle bin when `deleted=true`", body = inline(ListResponse<CronJob>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Scheduler error", body = ErrorBody),
    )
)]
async fn list_cron(
    State(state): State<AdminState>,
    Query(query): Query<ListCronQuery>,
) -> Result<Json<ListResponse<CronJob>>> {
    let jobs = if query.deleted {
        state.cron_scheduler.list_deleted_jobs().await
    } else {
        state.cron_scheduler.list_all_jobs().await
    }
    .map_err(cron_err)?;
    let items = jobs.into_iter().map(CronJob::from).collect();
    Ok(Json(ListResponse::new(items)))
}

#[utoipa::path(
    post,
    path = "/cron",
    tag = "cron",
    request_body = CreateCronRequest,
    responses(
        (status = 201, description = "Created cron job", body = CronJob),
        (status = 400, description = "Invalid schedule", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn create_cron(
    State(state): State<AdminState>,
    Json(req): Json<CreateCronRequest>,
) -> Result<(StatusCode, Json<CronJob>)> {
    let schedule = CronSchedule::cron(&req.schedule);
    let channel: ChannelTypeModel = req
        .channel
        .map(Into::into)
        .unwrap_or(ChannelTypeModel::http());
    let job = state
        .cron_scheduler
        .create_job(baybo_cron::NewCronJob {
            user_id: req.user_id,
            channel,
            title: req.title,
            schedule,
            prompt: req.text,
            timezone: req.timezone,
            origin_session_id: req.origin_session_id.map(Into::into),
        })
        .await
        .map_err(cron_err)?;
    Ok((StatusCode::CREATED, Json(CronJob::from(job))))
}

#[utoipa::path(
    get,
    path = "/cron/{id}",
    tag = "cron",
    params(
        ("id" = String, Path, description = "Cron job id"),
    ),
    responses(
        (status = 200, description = "Cron job record, deleted or live", body = CronJob),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
async fn get_cron(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Json<CronJob>> {
    let job = state
        .cron_scheduler
        .get_job(&id)
        .await
        .map_err(cron_err)?
        .ok_or_else(|| GatewayError::NotFound(format!("cron {id}")))?;
    Ok(Json(CronJob::from(job)))
}

#[utoipa::path(
    delete,
    path = "/cron/{id}",
    tag = "cron",
    params(
        ("id" = String, Path, description = "Cron job id"),
    ),
    responses(
        (status = 204, description = "Cron job moved to the recycle bin: it stops firing and leaves the default list, but the row survives — `GET /v1/cron/{id}` still resolves it, `GET /v1/cron?deleted=true` lists it, and `POST /v1/cron/{id}/restore` brings it back"),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 500, description = "Scheduler error", body = ErrorBody),
    )
)]
async fn delete_cron(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    state
        .cron_scheduler
        .delete_job(&id)
        .await
        .map_err(cron_err)?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/cron/{id}/pause",
    tag = "cron",
    params(
        ("id" = String, Path, description = "Cron job id"),
    ),
    responses(
        (status = 204, description = "Job paused: status is now `disabled` and it has no next trigger until resumed"),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 500, description = "Scheduler error", body = ErrorBody),
    )
)]
async fn pause_cron(State(state): State<AdminState>, Path(id): Path<String>) -> Result<StatusCode> {
    state
        .cron_scheduler
        .disable_job(&id)
        .await
        .map_err(cron_err)?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/cron/{id}/resume",
    tag = "cron",
    params(
        ("id" = String, Path, description = "Cron job id"),
    ),
    responses(
        (status = 204, description = "Job resumed: status is now `enabled` and the next trigger is computed from now — missed slots are not backfilled"),
        (status = 400, description = "Schedule has no future fire time (a one-shot whose moment has passed)", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 500, description = "Scheduler error", body = ErrorBody),
    )
)]
async fn resume_cron(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    state
        .cron_scheduler
        .enable_job(&id)
        .await
        .map_err(cron_err)?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/cron/{id}/restore",
    tag = "cron",
    params(
        ("id" = String, Path, description = "Cron job id"),
    ),
    responses(
        (status = 204, description = "Job restored from the recycle bin with the status it was deleted with; an enabled job's next trigger is recomputed from now, and a one-shot with no fire time left comes back paused"),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 500, description = "Scheduler error", body = ErrorBody),
    )
)]
async fn restore_cron(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    state
        .cron_scheduler
        .restore_job(&id)
        .await
        .map_err(cron_err)?;
    Ok(StatusCode::NO_CONTENT)
}
