//! `/v1/traces` — trace browser endpoints.
//!
//! Three surfaces:
//!
//! * `GET /v1/traces` — paginated, filtered list of session summaries
//!   used by the trace browser table. Typed via [`TraceSessionSummary`]
//!   so the OpenAPI/ts-rs surface picks it up.
//! * `GET /v1/traces/{session_id}` — session overview: the full
//!   `session_messages` log + job summaries (no step/span tree).
//!   The client lazily fetches each job's tree via the third endpoint.
//! * `GET /v1/traces/{session_id}/jobs/{job_id}` — per-job step/span
//!   tree. Spans carry `LlmCallInputs::Persisted` references in their
//!   original ordinal form; the client slices the message log it
//!   already has from the overview call.
//!
//! Both per-session endpoints stay untyped `serde_json::Value` because
//! the columnar Step/Span tree is polymorphic and re-mirroring the
//! closed enums in this crate would double the surface for no benefit;
//! the web client mirrors the trace types directly.

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
        .routes(routes!(get_job_trace))
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
        kind: q.kind.map(Into::into),
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
        ("session_id" = String, Path, description = "Session id whose trace overview to fetch"),
    ),
    responses(
        (
            status = 200,
            description = "Per-session trace overview: session_messages log + job summaries (no step/span data). Untyped JSON to keep the admin surface decoupled from internal trace crate changes.",
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
    let overview = state
        .query_api
        .load_trace_overview(&typed_session)
        .await
        .map_err(|e| GatewayError::Trace(e.to_string()))?;

    if overview.jobs.is_empty() {
        return Err(GatewayError::NotFound(format!("trace {session_id}")));
    }

    let jobs: Vec<serde_json::Value> = overview
        .jobs
        .iter()
        .map(|j| {
            let s = &j.summary;
            json!({
                "job_id": s.id.to_string(),
                "session_id": s.session_id.as_str(),
                "job_status_kind": s.status.kind().as_snake_case(),
                "created_at": s.created_at,
                "started_at": s.started_at,
                "ended_at": s.ended_at,
                "input_tokens": j.input_tokens,
                "output_tokens": j.output_tokens,
                "cached_input_tokens": j.cached_input_tokens,
                "cache_creation_input_tokens": j.cache_creation_input_tokens,
            })
        })
        .collect();

    Ok(Json(json!({
        "session_id": overview.session_id.as_str(),
        "session_messages": overview.session_messages,
        "jobs": jobs,
    })))
}

#[utoipa::path(
    get,
    path = "/traces/{session_id}/jobs/{job_id}",
    tag = "traces",
    params(
        ("session_id" = String, Path, description = "Session id this job belongs to (or inherits from); used for route scoping only"),
        ("job_id" = String, Path, description = "Job id whose step/span tree to fetch"),
    ),
    responses(
        (
            status = 200,
            description = "Per-job step/span tree. `LlmCall` spans keep `input_messages` as `{ last_ordinal: i64, prefix_len: usize, suffix?: ChatMessage[] }` (Persisted — `prefix_len` is a tripwire the client checks against the reconstructed count, `suffix` carries framing/sub-loop messages not in the log, e.g. compression & progress-observer spans) or `ChatMessage[]` (Inline); the client slices the message log it received from the overview call and appends any `suffix`.",
            body = serde_json::Value,
            content_type = "application/json",
        ),
        (status = 400, description = "Invalid job id", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "No such job", body = ErrorBody),
    )
)]
async fn get_job_trace(
    State(state): State<AdminState>,
    Path((_session_id, job_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>> {
    let parsed_job: aura_model::JobId = job_id
        .parse()
        .map_err(|e| GatewayError::BadRequest(format!("invalid job id: {e}")))?;
    let job_trace = state
        .query_api
        .load_job_trace(&parsed_job)
        .await
        .map_err(|e| GatewayError::Trace(e.to_string()))?;

    let steps: Vec<serde_json::Value> = job_trace
        .steps
        .iter()
        .map(|rs| json!({ "step": rs.step, "spans": rs.spans }))
        .collect();

    Ok(Json(json!({
        "job_id": job_trace.job.id.to_string(),
        "session_id": job_trace.job.session_id.as_str(),
        "job_status_kind": job_trace.job.status.kind().as_snake_case(),
        "created_at": job_trace.job.created_at,
        "started_at": job_trace.job.started_at,
        "ended_at": job_trace.job.ended_at,
        "steps": steps,
    })))
}
