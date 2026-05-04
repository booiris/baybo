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
    // Walk Jobs → Steps → Spans for this session.
    let jobs = state
        .job_lifecycle
        .list(None)
        .await
        .map_err(|e: aura_job::JobError| GatewayError::Trace(e.to_string()))?
        .into_iter()
        .filter(|j| j.session_id == typed_session)
        .collect::<Vec<_>>();

    if jobs.is_empty() {
        return Err(GatewayError::NotFound(format!("trace {session_id}")));
    }

    let mut job_blocks = Vec::with_capacity(jobs.len());
    for job in jobs {
        let steps = state
            .trace_store
            .list_steps_by_job(&job.id)
            .await
            .map_err(|e: aura_trace::TraceError| GatewayError::Trace(e.to_string()))?;
        let mut step_blocks = Vec::with_capacity(steps.len());
        for step in steps {
            let spans = state
                .trace_store
                .list_spans_by_step(&step.id)
                .await
                .map_err(|e: aura_trace::TraceError| GatewayError::Trace(e.to_string()))?;
            step_blocks.push(json!({ "step": step, "spans": spans }));
        }
        job_blocks.push(json!({
            "job_id": job.id.to_string(),
            "job_status_kind": job.status.kind().as_snake_case(),
            "steps": step_blocks,
        }));
    }

    Ok(Json(json!({
        "session_id": session_id,
        "jobs": job_blocks,
    })))
}
