//! `/v1/cron` endpoints.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;

use aura_cron::{CronJob, CronSchedule, TriggerAction};
use aura_model::ChannelType;

use crate::api::dto::{CreateCronRequest, ListResponse};
use crate::server::ApiState;
use crate::{GatewayError, Result};

pub fn routes() -> Router<ApiState> {
    Router::new()
        .route("/cron", get(list_cron).post(create_cron))
        .route("/cron/{id}", get(get_cron).delete(delete_cron))
}

async fn list_cron(State(state): State<ApiState>) -> Result<Json<ListResponse<CronJob>>> {
    let items = state
        .cron_scheduler
        .list_all_jobs()
        .await
        .map_err(|e| GatewayError::Cron(e.to_string()))?;
    Ok(Json(ListResponse::new(items)))
}

async fn create_cron(
    State(state): State<ApiState>,
    Json(req): Json<CreateCronRequest>,
) -> Result<(StatusCode, Json<CronJob>)> {
    let schedule = CronSchedule::cron(&req.schedule);
    let action = TriggerAction::Prompt { prompt: req.text };
    let channel = req.channel.unwrap_or(ChannelType::Http);
    let job = state
        .cron_scheduler
        .create_job(
            &req.user_id,
            channel,
            schedule,
            action,
            req.origin_session_id,
        )
        .await
        .map_err(|e| GatewayError::Cron(e.to_string()))?;
    Ok((StatusCode::CREATED, Json(job)))
}

async fn get_cron(State(state): State<ApiState>, Path(id): Path<String>) -> Result<Json<CronJob>> {
    let job = state
        .cron_scheduler
        .get_job(&id)
        .await
        .map_err(|e| GatewayError::Cron(e.to_string()))?
        .ok_or_else(|| GatewayError::NotFound(format!("cron {id}")))?;
    Ok(Json(job))
}

async fn delete_cron(State(state): State<ApiState>, Path(id): Path<String>) -> Result<StatusCode> {
    state
        .cron_scheduler
        .delete_job(&id)
        .await
        .map_err(|e| GatewayError::Cron(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}
