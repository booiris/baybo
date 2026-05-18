//! `/v1/traces` — trace browser endpoints.
//!
//! Two surfaces:
//!
//! * `GET /v1/traces` — paginated, filtered list of session summaries
//!   used by the trace browser table. Typed via [`TraceSessionSummary`]
//!   so the OpenAPI/ts-rs surface picks it up.
//! * `GET /v1/traces/{session_id}` — full per-session export. Stays
//!   untyped JSON because the columnar Step/Span tree is polymorphic
//!   and re-mirroring the closed enums in this crate would double the
//!   surface for no benefit; the web client mirrors the trace types
//!   directly.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde_json::json;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use aura_query::{SessionSummaryFilter, SessionSummaryPage};

use crate::api::dto::{ErrorBody, TraceSessionSummary, TracesListQuery, TracesListResponse};
use crate::server::AdminState;
use crate::{GatewayError, Result};

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 200;

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(list_traces))
        .routes(routes!(get_trace))
}

#[utoipa::path(
    get,
    path = "/traces",
    tag = "traces",
    params(TracesListQuery),
    responses(
        (status = 200, description = "Paginated session summaries (newest active first)", body = TracesListResponse),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn list_traces(
    State(state): State<AdminState>,
    Query(q): Query<TracesListQuery>,
) -> Result<Json<TracesListResponse>> {
    let filter = SessionSummaryFilter {
        status_kind: q.status.map(Into::into),
        since: q.since,
        until: q.until,
        session_id_prefix: q.q.clone(),
    };
    let page = SessionSummaryPage {
        offset: q.offset.unwrap_or(0),
        limit: q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
    };
    let listing = state
        .query_api
        .list_session_summaries(filter, page)
        .await
        .map_err(|e| GatewayError::Trace(e.to_string()))?;
    let items = listing
        .items
        .into_iter()
        .map(TraceSessionSummary::from)
        .collect();
    Ok(Json(TracesListResponse {
        items,
        total: listing.total,
    }))
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
                "created_at": rj.job.created_at,
                "started_at": rj.job.started_at,
                "ended_at": rj.job.ended_at,
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
