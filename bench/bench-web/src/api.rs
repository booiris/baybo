//! The read-only JSON API plus the embedded-UI fallback. Every handler
//! re-scans the bench filesystem, so a newly-finished run shows up on
//! the next request without a restart.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::get;
use serde::Deserialize;

use crate::adapters;
use crate::error::ApiError;
use crate::model::{BenchDetail, BenchInfo, RunDetail, SearchHit};
use crate::trace;

#[derive(Clone)]
struct AppState {
    root: Arc<PathBuf>,
}

/// Build the full router: `/api/*` JSON handlers + the React bundle as
/// the fallback for everything else.
pub fn router(root: PathBuf) -> Router {
    let state = AppState {
        root: Arc::new(root),
    };
    let api = Router::new()
        .route("/benches", get(list_benches))
        .route("/benches/{bench}", get(get_bench))
        .route("/benches/{bench}/runs/{run_key}", get(get_run))
        .route("/benches/{bench}/trace", get(get_trace))
        .route("/benches/{bench}/file", get(get_file))
        .route("/search", get(get_search))
        .with_state(state);
    Router::new()
        .nest("/api", api)
        .fallback(crate::webui::serve)
}

async fn list_benches(State(state): State<AppState>) -> Json<Vec<BenchInfo>> {
    Json(adapters::scan_benches(&state.root))
}

async fn get_bench(
    State(state): State<AppState>,
    Path(bench): Path<String>,
) -> Result<Json<BenchDetail>, ApiError> {
    adapters::bench_detail(&state.root, &bench)
        .map(Json)
        .ok_or(ApiError::NotFound)
}

async fn get_run(
    State(state): State<AppState>,
    Path((bench, run_key)): Path<(String, String)>,
) -> Result<Json<RunDetail>, ApiError> {
    adapters::run_detail(&state.root, &bench, &run_key)
        .map(Json)
        .ok_or(ApiError::NotFound)
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
}

async fn get_search(
    State(state): State<AppState>,
    Query(query): Query<SearchQuery>,
) -> Json<Vec<SearchHit>> {
    Json(adapters::search(&state.root, &query.q))
}

#[derive(Deserialize)]
struct TraceQuery {
    trace: String,
    #[serde(default)]
    messages: Option<String>,
}

async fn get_trace(
    State(state): State<AppState>,
    Path(bench): Path<String>,
    Query(query): Query<TraceQuery>,
) -> Result<axum::response::Response, ApiError> {
    let bench_dir = bench_dir(&state, &bench)?;
    trace::trace_response(&bench_dir, &query.trace, query.messages.as_deref())
}

#[derive(Deserialize)]
struct FileQuery {
    path: String,
}

async fn get_file(
    State(state): State<AppState>,
    Path(bench): Path<String>,
    Query(query): Query<FileQuery>,
) -> Result<axum::response::Response, ApiError> {
    let bench_dir = bench_dir(&state, &bench)?;
    trace::file_response(&bench_dir, &query.path)
}

/// Resolve `<root>/<bench>` only for a bench in the registry — rejects
/// an arbitrary first path segment before any file access.
fn bench_dir(state: &AppState, bench: &str) -> Result<PathBuf, ApiError> {
    let spec = adapters::spec(bench).ok_or(ApiError::NotFound)?;
    Ok(state.root.join(spec.id))
}
