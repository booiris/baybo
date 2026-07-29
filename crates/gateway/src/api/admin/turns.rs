//! `/v1/turns` endpoints.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use utoipa::IntoParams;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::api::dto::{BackgroundJob, BackgroundJobsResponse, ErrorBody, ListResponse, Turn};
use crate::server::AdminState;
use crate::{GatewayError, Result};

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(list_turns))
        .routes(routes!(get_turn))
        .routes(routes!(cancel_turn))
        .routes(routes!(list_background_jobs))
}

/// Default page size when `?limit=` is omitted. Picked to be small
/// enough that an unfiltered list on a large workspace doesn't page
/// the operator's terminal in one go, but large enough for normal
/// dashboards.
const DEFAULT_PAGE_LIMIT: usize = 50;
/// Hard ceiling so a malicious / mistyped `?limit=` can't allocate
/// arbitrary memory.
const MAX_PAGE_LIMIT: usize = 500;

#[derive(Debug, Deserialize, IntoParams)]
struct ListTurnsParams {
    /// Restrict to a single session. Hits the per-session index in
    /// the store instead of scanning the full turns table.
    session: Option<String>,
    /// Restrict to one terminal/in-flight status discriminator.
    /// Snake-case, matching `TurnStatusKind` (`pending`, `in_progress`,
    /// `stuck`, `cancelled`, `failed`, `completed`).
    status: Option<String>,
    /// Maximum items to return. Defaults to 50; capped at 500.
    limit: Option<usize>,
    /// Opaque cursor from a previous response's `next_cursor`.
    cursor: Option<String>,
}

fn parse_status(s: &str) -> Result<baybo_turn::TurnStatusKind> {
    use baybo_turn::TurnStatusKind;
    match s {
        "pending" => Ok(TurnStatusKind::Pending),
        "in_progress" => Ok(TurnStatusKind::InProgress),
        "stuck" => Ok(TurnStatusKind::Stuck),
        "cancelled" => Ok(TurnStatusKind::Cancelled),
        "failed" => Ok(TurnStatusKind::Failed),
        "completed" => Ok(TurnStatusKind::Completed),
        other => Err(GatewayError::BadRequest(format!(
            "invalid status filter: {other:?}"
        ))),
    }
}

#[utoipa::path(
    get,
    path = "/turns",
    tag = "turns",
    params(ListTurnsParams),
    responses(
        (status = 200, description = "Paginated turns", body = inline(ListResponse<Turn>)),
        (status = 400, description = "Invalid query parameters", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Turn store error", body = ErrorBody),
    )
)]
async fn list_turns(
    State(state): State<AdminState>,
    Query(params): Query<ListTurnsParams>,
) -> Result<Json<ListResponse<Turn>>> {
    let status = params.status.as_deref().map(parse_status).transpose()?;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_PAGE_LIMIT)
        .clamp(1, MAX_PAGE_LIMIT);
    let offset: usize = match params.cursor.as_deref() {
        Some(c) => c
            .parse()
            .map_err(|_| GatewayError::BadRequest(format!("invalid cursor: {c:?}")))?,
        None => 0,
    };

    let (page_turns, total) = match params.session {
        // Session-scoped lists stay a per-session load (bounded by that
        // session's turn count) with in-memory paging.
        Some(sid) => {
            let sid = baybo_model::SessionId::from(sid);
            let mut turns = state
                .turn_lifecycle
                .list_by_session(&sid, status)
                .await
                .map_err(|e: baybo_turn::TurnError| GatewayError::Turn(e.to_string()))?;
            let total = turns.len();
            if offset >= total {
                return Ok(Json(ListResponse::new(Vec::new())));
            }
            let page: Vec<_> = turns.drain(offset..).take(limit).collect();
            (page, total)
        }
        // The unscoped list pages in SQL — a page never materialises
        // the whole turns table.
        None => state
            .turn_lifecycle
            .list_page(status, limit, offset)
            .await
            .map_err(|e: baybo_turn::TurnError| GatewayError::Turn(e.to_string()))?,
    };
    let page: Vec<_> = page_turns.into_iter().map(Turn::from).collect();
    let next_cursor = if offset + page.len() < total {
        Some((offset + page.len()).to_string())
    } else {
        None
    };
    Ok(Json(ListResponse::with_next_cursor(page, next_cursor)))
}

/// Cross-session view of in-flight background jobs (detached subagents +
/// `Bash` commands) — the dashboard twin of the per-session `JobList` tool.
#[utoipa::path(
    get,
    path = "/background-jobs",
    tag = "jobs",
    responses(
        (status = 200, description = "In-flight background jobs", body = BackgroundJobsResponse),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn list_background_jobs(State(state): State<AdminState>) -> Json<BackgroundJobsResponse> {
    let jobs = state
        .supervisor
        .list_all_in_flight_background()
        .into_iter()
        .map(|(parent, info)| BackgroundJob {
            handle: info.handle,
            session_id: parent.as_ref().to_string(),
            kind: info.kind,
            summary: info.task_summary,
        })
        .collect();
    Json(BackgroundJobsResponse { jobs })
}

#[utoipa::path(
    get,
    path = "/turns/{id}",
    tag = "turns",
    params(
        ("id" = String, Path, description = "Turn id"),
    ),
    responses(
        (status = 200, description = "Turn record", body = Turn),
        (status = 400, description = "Invalid turn id", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Not found", body = ErrorBody),
    )
)]
async fn get_turn(State(state): State<AdminState>, Path(id): Path<String>) -> Result<Json<Turn>> {
    let turn_id: baybo_model::TurnId = id
        .parse()
        .map_err(|e| GatewayError::BadRequest(format!("invalid turn id: {e}")))?;
    let turn = state
        .turn_lifecycle
        .get(&turn_id)
        .await
        .map_err(|e: baybo_turn::TurnError| GatewayError::Turn(e.to_string()))?
        .ok_or_else(|| GatewayError::NotFound(format!("turn {id}")))?;
    Ok(Json(Turn::from(turn)))
}

#[utoipa::path(
    post,
    path = "/turns/{id}/cancel",
    tag = "turns",
    params(
        ("id" = String, Path, description = "Turn id"),
    ),
    responses(
        (status = 200, description = "Cancelled turn record", body = Turn),
        (status = 400, description = "Invalid turn id", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Not found", body = ErrorBody),
        (status = 500, description = "Cancel failed", body = ErrorBody),
    )
)]
async fn cancel_turn(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Json<Turn>> {
    let turn_id: baybo_model::TurnId = id
        .parse()
        .map_err(|e| GatewayError::BadRequest(format!("invalid turn id: {e}")))?;
    // Operator-initiated cancel; partial-artifact rollup belongs on the
    // recovery scan, not the admin endpoint.
    state
        .turn_lifecycle
        .cancel(&turn_id, baybo_turn::CancelReason::OperatorCancel, vec![])
        .await
        .map_err(|e: baybo_turn::TurnError| GatewayError::Turn(e.to_string()))?;
    let turn = state
        .turn_lifecycle
        .get(&turn_id)
        .await
        .map_err(|e: baybo_turn::TurnError| GatewayError::Turn(e.to_string()))?
        .ok_or_else(|| GatewayError::NotFound(format!("turn {id}")))?;
    Ok(Json(Turn::from(turn)))
}
