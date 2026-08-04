//! `/v1/traces` — trace browser endpoints.
//!
//! Four surfaces:
//!
//! * `GET /v1/traces` — paginated, filtered list of session summaries
//!   used by the trace browser table. Typed via [`TraceSessionSummary`]
//!   so the OpenAPI/ts-rs surface picks it up.
//! * `GET /v1/traces/{session_id}` — session overview: the full
//!   `session_messages` log + turn summaries (no step/span tree).
//!   The client lazily fetches each turn's tree via the per-turn endpoint.
//! * `GET /v1/traces/{session_id}/lineage` — the subagent sessions
//!   descended from this one, so the viewer can nest a child's trace
//!   under the `spawn_subagent` span that started it instead of making
//!   the reader navigate away. Summaries only; the child's own overview
//!   and turn trees are fetched lazily through the endpoints above.
//! * `GET /v1/traces/{session_id}/turns/{turn_id}` — per-turn step/span
//!   tree. Spans carry `LlmCallInputs::Persisted` (by ordinal) and
//!   `ToolCallOutput::Persisted` (by `tool_use_id`) references unresolved;
//!   the client resolves them against the message log it already has from the
//!   overview call.
//!
//! The three per-session endpoints stay untyped `serde_json::Value` because
//! the columnar Step/Span tree is polymorphic and re-mirroring the
//! closed enums in this crate would double the surface for no benefit;
//! the web client mirrors the trace types directly.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde_json::json;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use baybo_query::{SessionSummaryFilter, SessionSummaryPage};

use crate::api::dto::{
    ErrorBody, TraceOverviewQuery, TraceSessionSummary, TracesListQuery, TracesListResponse,
};
use crate::server::AdminState;
use crate::{GatewayError, Result};

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 200;

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(list_traces))
        .routes(routes!(get_trace))
        .routes(routes!(get_trace_lineage))
        .routes(routes!(get_turn_trace))
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
        TraceOverviewQuery,
    ),
    responses(
        (
            status = 200,
            description = "Per-session trace overview: session_messages log + turn summaries (no step/span data). With `since_ordinal`, `session_messages` carries only rows above that ordinal; `supersede_watermark` tells the client when its cached prefix went stale (compaction) and a full reload is needed. Untyped JSON to keep the admin surface decoupled from internal trace crate changes.",
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
    Query(q): Query<TraceOverviewQuery>,
) -> Result<Json<serde_json::Value>> {
    let typed_session = baybo_model::SessionId::from(session_id.as_str());
    let overview = state
        .query_api
        .load_trace_overview(&typed_session, q.since_ordinal)
        .await
        .map_err(|e| GatewayError::Trace(e.to_string()))?;

    if overview.turns.is_empty() {
        return Err(GatewayError::NotFound(format!("trace {session_id}")));
    }

    Ok(Json(json!({
        "session_id": overview.session_id.as_str(),
        "session_messages": overview.session_messages,
        "turns": turn_summaries_json(&overview.turns),
        "supersede_watermark": overview.supersede_watermark,
        "external_agent": overview.external_agent,
        "subagent_type": overview.subagent_type,
    })))
}

/// The turn-summary projection shared by the overview and lineage
/// bodies, so a child session's turn rows are byte-identical to the
/// root's and the client can render both through one type.
fn turn_summaries_json(turns: &[baybo_query::TraceTurnSummary]) -> Vec<serde_json::Value> {
    turns
        .iter()
        .map(|j| {
            let s = &j.summary;
            json!({
                "turn_id": s.id.to_string(),
                "session_id": s.session_id.as_str(),
                "turn_status_kind": s.status.kind().as_snake_case(),
                "turn_input_kind": s.input_kind,
                "created_at": s.created_at,
                "started_at": s.started_at,
                "ended_at": s.ended_at,
                "input_tokens": j.input_tokens,
                "output_tokens": j.output_tokens,
                "cached_input_tokens": j.cached_input_tokens,
                "cache_creation_input_tokens": j.cache_creation_input_tokens,
            })
        })
        .collect()
}

#[utoipa::path(
    get,
    path = "/traces/{session_id}/lineage",
    tag = "traces",
    params(
        ("session_id" = String, Path, description = "Session id whose subagent descendants to fetch"),
    ),
    responses(
        (
            status = 200,
            description = "Every subagent session descended from this one, flattened. Each row carries its attach point (`parent_span_id` — the parent's `spawn_subagent` tool-call span), the external-agent backend that ran it (`external_agent`, absent for in-process children), and its turn summaries in the same shape as the overview's `turns`. No step/span tree: the client fetches a child's turn trees lazily through the per-turn endpoint, and an external child has no tree at all. Untyped JSON, consistent with the rest of the per-session traces family.",
            body = serde_json::Value,
            content_type = "application/json",
        ),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn get_trace_lineage(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let typed_session = baybo_model::SessionId::from(session_id.as_str());
    let sessions = state
        .query_api
        .load_lineage_overview(&typed_session)
        .await
        .map_err(|e| GatewayError::Trace(e.to_string()))?;

    let rows: Vec<serde_json::Value> = sessions
        .iter()
        .map(|c| {
            json!({
                "session_id": c.session_id.as_str(),
                "parent_session_id": c.parent_session_id.as_str(),
                "parent_turn_id": c.parent_turn_id.to_string(),
                "parent_span_id": c.parent_span_id.map(|id| id.to_string()),
                "external_agent": c.external_agent,
                "subagent_type": c.subagent_type,
                "turns": turn_summaries_json(&c.turns),
            })
        })
        .collect();

    Ok(Json(json!({
        "root_session_id": typed_session.as_str(),
        "sessions": rows,
    })))
}

#[utoipa::path(
    get,
    path = "/traces/{session_id}/turns/{turn_id}",
    tag = "traces",
    params(
        ("session_id" = String, Path, description = "Session id this turn belongs to (or inherits from); used for route scoping only"),
        ("turn_id" = String, Path, description = "Turn id whose step/span tree to fetch"),
    ),
    responses(
        (
            status = 200,
            description = "Per-turn step/span tree. `LlmCall` spans keep `input_messages` as `{ last_ordinal: i64, prefix_len: usize, suffix?: ChatMessage[] }` (Persisted) or `ChatMessage[]` (Inline). A larger `ToolCall.result.output` may be `{ $baybo_ref: 'session_tool_result', tool_use_id, attachments?, llm_images? }`; the client resolves both reference kinds against the session message log returned by the overview call, a persisted tool output to its transcript row's `ToolResult` content by `tool_use_id`.",
            body = serde_json::Value,
            content_type = "application/json",
        ),
        (status = 400, description = "Invalid turn id", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "No such turn", body = ErrorBody),
    )
)]
async fn get_turn_trace(
    State(state): State<AdminState>,
    Path((_session_id, turn_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>> {
    let parsed_turn: baybo_model::TurnId = turn_id
        .parse()
        .map_err(|e| GatewayError::BadRequest(format!("invalid turn id: {e}")))?;
    let turn_trace = state
        .query_api
        .load_turn_trace(&parsed_turn)
        .await
        .map_err(|e| GatewayError::Trace(e.to_string()))?;

    let steps: Vec<serde_json::Value> = turn_trace
        .steps
        .iter()
        .map(|rs| json!({ "step": rs.step, "spans": rs.spans }))
        .collect();

    Ok(Json(json!({
        "turn_id": turn_trace.turn.id.to_string(),
        "session_id": turn_trace.turn.session_id.as_str(),
        "turn_status_kind": turn_trace.turn.status.kind().as_snake_case(),
        "turn_input_kind": turn_trace.turn.input_kind(),
        "created_at": turn_trace.turn.created_at,
        "started_at": turn_trace.turn.started_at,
        "ended_at": turn_trace.turn.ended_at,
        "steps": steps,
    })))
}
