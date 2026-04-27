//! `/v1/sessions` endpoints.

use axum::Json;
use axum::extract::{Path, State};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::api::dto::{ErrorBody, ForkSessionRequest, ListResponse, SessionDetail, SessionSummary};
use crate::server::AdminState;
use crate::{GatewayError, Result};

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(list_sessions))
        .routes(routes!(get_session))
        .routes(routes!(fork_session))
}

#[utoipa::path(
    get,
    path = "/sessions",
    tag = "sessions",
    responses(
        (status = 200, description = "All sessions, newest-active first.", body = inline(ListResponse<SessionSummary>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Session store error", body = ErrorBody),
    )
)]
async fn list_sessions(
    State(state): State<AdminState>,
) -> Result<Json<ListResponse<SessionSummary>>> {
    let items = state
        .session_manager
        .list()
        .await
        .map_err(|e| GatewayError::Session(e.to_string()))?
        .iter()
        .map(SessionSummary::from)
        .collect();
    Ok(Json(ListResponse::new(items)))
}

#[utoipa::path(
    get,
    path = "/sessions/{id}",
    tag = "sessions",
    params(
        ("id" = String, Path, description = "Session id"),
    ),
    responses(
        (status = 200, description = "Session detail (metadata only — transcript intentionally omitted; pull the trace for call-chain content).", body = SessionDetail),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
async fn get_session(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Json<SessionDetail>> {
    let session = state
        .session_manager
        .get(&id)
        .await
        .map_err(|e| GatewayError::Session(e.to_string()))?
        .ok_or_else(|| GatewayError::NotFound(format!("session {id}")))?;
    Ok(Json(SessionDetail::from(session)))
}

#[utoipa::path(
    post,
    path = "/sessions/{id}/fork",
    tag = "sessions",
    params(
        ("id" = String, Path, description = "Parent session id"),
    ),
    request_body = ForkSessionRequest,
    responses(
        (status = 200, description = "Newly created child session.", body = SessionDetail),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Parent session or referenced job not found", body = ErrorBody),
        (status = 500, description = "Fork failed", body = ErrorBody),
    )
)]
async fn fork_session(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(body): Json<ForkSessionRequest>,
) -> Result<Json<SessionDetail>> {
    // Validate the referenced job actually exists in the parent's job
    // history before forking. The session crate intentionally has no
    // job-store dep, so this gate lives here.
    let job = state
        .job_manager
        .get(&body.at_job_id)
        .await
        .map_err(|e| GatewayError::Job(e.to_string()))?
        .ok_or_else(|| GatewayError::NotFound(format!("job {}", body.at_job_id)))?;
    if job.session_id != id {
        return Err(GatewayError::NotFound(format!(
            "job {} does not belong to session {}",
            body.at_job_id, id
        )));
    }

    let child = state
        .session_manager
        .fork_session(&id, &body.at_job_id)
        .await
        .map_err(|e| GatewayError::Session(e.to_string()))?;
    Ok(Json(SessionDetail::from(child)))
}
