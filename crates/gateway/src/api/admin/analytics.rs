//! `/v1/analytics` — aggregated cost + session activity for the
//! analytics dashboard.
//!
//! Defaults to the last 30 UTC days when no range is supplied. Caller
//! supplies `since` / `until` for any other window. Results are
//! computed via [`aura_query::QueryApi::compute_analytics`], which
//! single-passes `cost_records` plus a session list.

use axum::Json;
use axum::extract::{Query, State};
use chrono::{Duration, Utc};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use aura_cost::TimeRange;

use crate::api::dto::{AnalyticsQuery, AnalyticsResponse, ErrorBody};
use crate::server::AdminState;
use crate::{GatewayError, Result};

const DEFAULT_DAYS: i64 = 30;

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new().routes(routes!(get_analytics))
}

#[utoipa::path(
    get,
    path = "/analytics",
    tag = "analytics",
    params(AnalyticsQuery),
    responses(
        (status = 200, description = "Aggregated tokens / cost / sessions over the time range", body = AnalyticsResponse),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn get_analytics(
    State(state): State<AdminState>,
    Query(q): Query<AnalyticsQuery>,
) -> Result<Json<AnalyticsResponse>> {
    let until = q.until.unwrap_or_else(Utc::now);
    let since = q.since.unwrap_or(until - Duration::days(DEFAULT_DAYS));
    if since >= until {
        return Err(GatewayError::BadRequest(
            "since must be strictly less than until".into(),
        ));
    }
    let summary = state
        .query_api
        .compute_analytics(TimeRange {
            from: since,
            to: until,
        })
        .await
        .map_err(|e| GatewayError::Trace(e.to_string()))?;

    Ok(Json(AnalyticsResponse {
        since,
        until,
        total_input_tokens: summary.total_input_tokens,
        total_output_tokens: summary.total_output_tokens,
        total_cached_input_tokens: summary.total_cached_input_tokens,
        total_cache_creation_input_tokens: summary.total_cache_creation_input_tokens,
        total_cost_micro_usd: summary.total_cost_usd,
        total_record_count: summary.total_record_count,
        daily: summary.daily.into_iter().map(Into::into).collect(),
        by_model: summary.by_model.into_iter().map(Into::into).collect(),
    }))
}
