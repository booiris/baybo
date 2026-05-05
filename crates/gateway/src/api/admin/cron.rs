//! `/v1/cron` endpoints.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use aura_cron::{CronSchedule, TriggerAction};
use aura_model::ChannelType as ChannelTypeModel;

use crate::api::dto::{CreateCronRequest, CronJob, ErrorBody, ListResponse};
use crate::server::AdminState;
use crate::{GatewayError, Result};

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(list_cron, create_cron))
        .routes(routes!(get_cron, delete_cron))
}

#[utoipa::path(
    get,
    path = "/cron",
    tag = "cron",
    responses(
        (status = 200, description = "All cron jobs", body = inline(ListResponse<CronJob>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Scheduler error", body = ErrorBody),
    )
)]
async fn list_cron(State(state): State<AdminState>) -> Result<Json<ListResponse<CronJob>>> {
    let items = state
        .cron_scheduler
        .list_all_jobs()
        .await
        .map_err(|e| GatewayError::Cron(e.to_string()))?
        .into_iter()
        .map(CronJob::from)
        .collect();
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
    let action = TriggerAction::Prompt { prompt: req.text };
    let channel: ChannelTypeModel = req
        .channel
        .map(Into::into)
        .unwrap_or(ChannelTypeModel::http());
    let job = state
        .cron_scheduler
        .create_job(
            &req.user_id,
            channel,
            schedule,
            action,
            req.timezone,
            req.origin_session_id,
        )
        .await
        .map_err(|e| GatewayError::Cron(e.to_string()))?;
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
        (status = 200, description = "Cron job record", body = CronJob),
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
        .map_err(|e| GatewayError::Cron(e.to_string()))?
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
        (status = 204, description = "Cron job deleted"),
        (status = 401, description = "Unauthorized", body = ErrorBody),
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
        .map_err(|e| GatewayError::Cron(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}
