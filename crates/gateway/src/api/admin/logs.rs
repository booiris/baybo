//! `/v1/logs` endpoint — read-only view into the in-memory `LogBuffer`.
//!
//! The gateway installs a [`LogBufferLayer`] on the tracing dispatcher
//! so every event at or above the env-filter level lands in a bounded
//! ring buffer. This handler just forwards the client's filters to
//! `LogBuffer::query` and mirrors the result through the admin DTOs.
//!
//! The buffer does not persist across restarts — it's a live
//! observability aid, not an audit log. The rolling file under
//! `<workspace>/logs/aura.log` remains the durable record.
//!
//! [`LogBufferLayer`]: crate::log_buffer::LogBufferLayer

use axum::Json;
use axum::extract::{Query, State};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::Result;
use crate::api::dto::{ErrorBody, LogEntry, LogsQuery, LogsResponse};
use crate::log_buffer::LogQuery;
use crate::server::AdminState;

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new().routes(routes!(list_logs))
}

#[utoipa::path(
    get,
    path = "/logs",
    tag = "logs",
    params(LogsQuery),
    responses(
        (status = 200, description = "Page of recent log records, newest first", body = LogsResponse),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn list_logs(
    State(state): State<AdminState>,
    Query(q): Query<LogsQuery>,
) -> Result<Json<LogsResponse>> {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = q.offset.unwrap_or(0);
    let query = LogQuery {
        level: q.level.map(Into::into),
        q: q.q.clone(),
        since: q.since,
        until: q.until,
        limit,
        offset,
    };
    let page = state.log_buffer.query(&query);
    let items = page.items.into_iter().map(LogEntry::from).collect();
    Ok(Json(LogsResponse {
        items,
        total: page.total,
    }))
}
