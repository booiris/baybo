//! `GET /v1/config` — read-only snapshot of the loaded `AuraConfig`.
//!
//! The response is the raw config with api-key fields redacted by the
//! underlying `serde` impls (which skip secret fields) — keep secret
//! handling centralised in `aura_config`, not here.

use axum::Router;
use axum::extract::State;
use axum::routing::get;

use crate::Result;
use crate::server::ApiState;

pub fn routes() -> Router<ApiState> {
    Router::new().route("/config", get(get_config))
}

async fn get_config(State(state): State<ApiState>) -> Result<axum::Json<serde_json::Value>> {
    let value = serde_json::to_value(&*state.config)
        .map_err(|e| crate::GatewayError::Internal(e.to_string()))?;
    Ok(axum::Json(value))
}
