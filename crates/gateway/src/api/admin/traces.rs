//! `/v1/traces/{session_id}` — session trace export.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::routing::get;

use crate::server::AdminState;
use crate::{GatewayError, Result};

pub fn routes() -> Router<AdminState> {
    Router::new().route("/traces/{session_id}", get(get_trace))
}

async fn get_trace(
    State(state): State<AdminState>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let trace = state
        .trace_store
        .load_trace(&session_id)
        .await
        .map_err(|e| GatewayError::Trace(e.to_string()))?
        .ok_or_else(|| GatewayError::NotFound(format!("trace {session_id}")))?;
    let value = serde_json::to_value(&trace).map_err(|e| GatewayError::Internal(e.to_string()))?;
    Ok(Json(value))
}
