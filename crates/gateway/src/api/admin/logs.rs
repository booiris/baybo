//! `/v1/logs` endpoints — paged snapshot and live SSE stream into the
//! in-memory `LogBuffer`.
//!
//! The gateway installs a [`LogBufferLayer`] on the tracing dispatcher
//! so every event at or above the env-filter level lands in a bounded
//! ring buffer. `GET /v1/logs` forwards the client's filters to
//! `LogBuffer::query` for pagination; `GET /v1/logs/stream` subscribes
//! to the same buffer's `broadcast::Sender` so newly captured records
//! are pushed as SSE events — the webui uses this for "Live" mode
//! instead of polling.
//!
//! The buffer does not persist across restarts — it's a live
//! observability aid, not an audit log. The rolling file under
//! `<workspace>/logs/baybo.log` remains the durable record.
//!
//! [`LogBufferLayer`]: crate::log_buffer::LogBufferLayer

use std::convert::Infallible;
use std::time::Duration;

use async_stream::try_stream;
use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::{Json, response::IntoResponse};
use futures::Stream;
use tokio::sync::broadcast::error::RecvError;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::Result;
use crate::api::dto::{ErrorBody, LogEntry, LogsQuery, LogsResponse};
use crate::log_buffer::LogQuery;
use crate::server::AdminState;

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 500;

/// Ping frequency for `/v1/logs/stream`. Keeps idle connections open
/// through proxies that close silent sockets.
const STREAM_KEEPALIVE: Duration = Duration::from_secs(15);

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(list_logs))
        .routes(routes!(stream_logs))
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
    let query = to_log_query(&q);
    let page = state.log_buffer.query(&query);
    let items = page.items.into_iter().map(LogEntry::from).collect();
    Ok(Json(LogsResponse {
        items,
        total: page.total,
    }))
}

#[utoipa::path(
    get,
    path = "/logs/stream",
    tag = "logs",
    params(LogsQuery),
    responses(
        (
            status = 200,
            description = "Server-Sent Events stream of LogEntry JSON — one `log` event per newly captured record matching the filters. Emits `lagged` if the subscriber can't keep up.",
            content_type = "text/event-stream",
        ),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn stream_logs(
    State(state): State<AdminState>,
    Query(q): Query<LogsQuery>,
) -> impl IntoResponse {
    let query = to_log_query(&q);
    let mut rx = state.log_buffer.subscribe();

    let stream: std::pin::Pin<
        Box<dyn Stream<Item = std::result::Result<Event, Infallible>> + Send>,
    > = Box::pin(try_stream! {
        // Flush something immediately so the browser's EventSource
        // fires `open` before the first real log event. Without this,
        // idle streams stay in "Connecting…" until the first record
        // happens to arrive — which can be minutes on a quiet system.
        yield Event::default().comment("ready");
        loop {
            match rx.recv().await {
                Ok(record) => {
                    if !record.matches(&query) {
                        continue;
                    }
                    let entry = LogEntry::from(record);
                    match Event::default().event("log").json_data(&entry) {
                        Ok(evt) => yield evt,
                        Err(e) => {
                            yield Event::default()
                                .event("error")
                                .data(format!("encode error: {e}"));
                            break;
                        }
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    yield Event::default()
                        .event("lagged")
                        .data(skipped.to_string());
                }
                // Sender dropped — buffer gone. Signal EOF and bail.
                Err(RecvError::Closed) => {
                    yield Event::default().event("end").data("");
                    break;
                }
            }
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::new().interval(STREAM_KEEPALIVE).text("ping"))
}

fn to_log_query(q: &LogsQuery) -> LogQuery {
    LogQuery {
        level: q.level.map(Into::into),
        q: q.q.clone(),
        since: q.since,
        until: q.until,
        limit: q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
        offset: q.offset.unwrap_or(0),
    }
}
