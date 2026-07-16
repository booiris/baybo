//! `/v1/cron` endpoints.

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use utoipa::IntoParams;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use baybo_cron::{CronError, CronSchedule};
use baybo_model::ChannelType as ChannelTypeModel;

use crate::api::admin::chat::{broadcast_session_list_stale, chat_list_channel};
use crate::api::dto::{
    CreateCronRequest, CronJob, ErrorBody, ListResponse, SetCronPinRequest, UpdateCronRequest,
};
use crate::auth::AuthedClient;
use crate::server::AdminState;
use crate::{GatewayError, Result};

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(list_cron, create_cron))
        .routes(routes!(get_cron, update_cron, delete_cron))
        .routes(routes!(set_cron_pin))
        .routes(routes!(pause_cron))
        .routes(routes!(resume_cron))
        .routes(routes!(restore_cron))
}

/// Map a scheduler error onto the right HTTP status: an unknown job id is
/// 404, a request the caller got wrong (a schedule with no future fire time —
/// resuming or rescheduling onto a one-shot whose moment has passed — or an
/// edit that sets no field) is 400, an edit that kept losing to a concurrent
/// write is a retryable 409, anything else is a 500.
fn cron_err(e: CronError) -> GatewayError {
    match e {
        CronError::NotFound(m) => GatewayError::NotFound(m),
        caller_bug @ (CronError::InvalidSchedule(_)
        | CronError::EmptyUpdate(_)
        | CronError::BlankPrompt) => GatewayError::BadRequest(caller_bug.to_string()),
        contended @ CronError::Contended(_) => GatewayError::Conflict(contended.to_string()),
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
        (status = 400, description = "Invalid schedule, or a blank prompt", body = ErrorBody),
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
        .unwrap_or_else(ChannelTypeModel::owner);
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
    patch,
    path = "/cron/{id}",
    tag = "cron",
    params(
        ("id" = String, Path, description = "Cron job id"),
    ),
    request_body = UpdateCronRequest,
    responses(
        (status = 200, description = "The edited job, with the `next_trigger_at` the edit produced — a client need not refetch. Fields the body left out are unchanged. Editing the schedule or the timezone recomputes the next fire time from now: the slots the job missed are never back-filled. A paused job stays paused — it keeps `disabled` and no next trigger until `POST /v1/cron/{id}/resume` — while a one-shot that already fired is re-armed by an `at` still in the future", body = CronJob),
        (status = 400, description = "The body sets no field, blanks the prompt, or carries a schedule with no future fire time (an `at` whose moment has passed)", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "No such job, or the job is in the recycle bin — `POST /v1/cron/{id}/restore` it before editing it", body = ErrorBody),
        (status = 409, description = "The edit kept losing to a concurrent write (the job fired while it was being applied). Nothing changed; retry", body = ErrorBody),
        (status = 500, description = "Scheduler error", body = ErrorBody),
    )
)]
async fn update_cron(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateCronRequest>,
) -> Result<Json<CronJob>> {
    let job = state
        .cron_scheduler
        .update_job(&id, req.into())
        .await
        .map_err(cron_err)?;
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
        (status = 409, description = "The pause kept losing to a concurrent write (the job fired, or was edited, while it was being applied). Nothing changed; retry", body = ErrorBody),
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
        (status = 409, description = "The resume kept losing to a concurrent write (the job was edited while it was being applied). Nothing changed; retry", body = ErrorBody),
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

#[utoipa::path(
    put,
    path = "/cron/{id}/pin",
    tag = "cron",
    params(
        ("id" = String, Path, description = "Cron job id"),
    ),
    request_body = SetCronPinRequest,
    responses(
        (status = 204, description = "Pin state updated"),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
async fn set_cron_pin(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    authed: Option<Extension<AuthedClient>>,
    Json(req): Json<SetCronPinRequest>,
) -> Result<StatusCode> {
    // The FIRST mutation a client can make to a cron job, so it is also the
    // first that needs scoping — and unlike the chat routes there was no
    // precedent here to copy (`list_cron` is deliberately unfiltered, an
    // operator surface). A job carries the same `ChannelType` as a session, and
    // `http` (web) and `device` (iOS) own disjoint universes, so a job from the
    // other universe must be indistinguishable from one that does not exist.
    let authed = authed.as_ref().map(|ext| &ext.0);
    let not_found = || GatewayError::NotFound(format!("cron {id}"));
    let job = state
        .cron_scheduler
        .get_job(&id)
        .await
        .map_err(|e| GatewayError::Cron(e.to_string()))?
        .ok_or_else(not_found)?;
    // Channel-scoped like the chat gates: a chat caller operates on the
    // `owner` channel, so only `owner` cron jobs are reachable; `tui` /
    // `Multiplexed` jobs stay isolated.
    if job.channel != chat_list_channel(authed) {
        return Err(not_found());
    }
    // Targeted flat-column write, NOT get→mutate→save: `save` rewrites the whole
    // row from its caller's snapshot and the scheduler re-saves on every fire, so
    // the read-modify-write shape would lose this pin on the job's next tick.
    if !state
        .cron_scheduler
        .set_job_pinned(&id, req.pinned)
        .await
        .map_err(|e| GatewayError::Cron(e.to_string()))?
    {
        return Err(not_found());
    }
    // The group is a VIEW over the job's fire sessions, so there is no session
    // row whose `pinned` changed and nothing for `SessionPatch` to carry — every
    // client re-derives the group's block from `cron_group_pinned` on its next
    // list pull. `list_stale` is the existing nudge for exactly that.
    broadcast_session_list_stale(&state, &job.channel);
    Ok(StatusCode::NO_CONTENT)
}
