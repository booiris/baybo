//! `/v1/traces/{session_id}` — session trace export.
//!
//! Exposes the columnar trace tree as `{ steps: [...], spans_by_step: {...} }`.
//! Historic `load_trace` returned a `SessionTrace` tree; the new model
//! is `Job > Step > Span (+ SpanEvent)` so the wire shape changed
//! accordingly. The endpoint is read-only and stays untyped JSON to
//! avoid recreating Step/Span DTOs in this crate.

use axum::Json;
use axum::extract::{Path, State};
use serde_json::json;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::api::dto::ErrorBody;
use crate::server::AdminState;
use crate::{GatewayError, Result};

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new().routes(routes!(get_trace))
}

#[utoipa::path(
    get,
    path = "/traces/{session_id}",
    tag = "traces",
    params(
        ("session_id" = String, Path, description = "Session id whose trace to export"),
    ),
    responses(
        (
            status = 200,
            description = "Per-session trace tree: jobs, their steps, and the spans under each step. Untyped JSON to keep the admin surface decoupled from internal trace crate changes.",
            body = serde_json::Value,
            content_type = "application/json",
        ),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "No trace for that session", body = ErrorBody),
    )
)]
async fn get_trace(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let typed_session = aura_model::SessionId::from(session_id.as_str());
    let replay = state
        .query_api
        .replay(&typed_session, None)
        .await
        .map_err(|e| GatewayError::Trace(e.to_string()))?;

    if replay.jobs.is_empty() {
        return Err(GatewayError::NotFound(format!("trace {session_id}")));
    }

    let job_blocks: Vec<serde_json::Value> = replay
        .jobs
        .iter()
        .map(|rj| {
            json!({
                "job_id": rj.job.id.to_string(),
                "job_status_kind": rj.job.status.kind().as_snake_case(),
                "steps": rj
                    .steps
                    .iter()
                    .map(|rs| json!({ "step": rs.step, "spans": rs.spans }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();

    Ok(Json(json!({
        "session_id": session_id,
        "jobs": job_blocks,
    })))
}
