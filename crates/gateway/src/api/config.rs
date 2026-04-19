//! `/v1/config` — read and mutate the loaded `AuraConfig`.
//!
//! `GET` returns a snapshot of the in-memory config (with secret fields
//! redacted by `aura_config`'s serde impls). `PUT` / `DELETE` write
//! through to the same on-disk `aura.json` that `aura config set/unset`
//! targets: we do not mutate the running process's `Arc<AuraConfig>` in
//! place, so callers must restart the gateway to pick the change up.
//! This matches the CLI's semantics (see `crates/cli/src/commands/config.rs`)
//! and sidesteps having to wrap every manager's config snapshot behind
//! an `ArcSwap`.
//!
//! Both mutation endpoints fail with `BadRequest` when the gateway was
//! started without a config file (pure-default boot) — there is no
//! destination to write the change to.
//!
//! The body shape matches the CLI:
//!   * `PUT  { "path": "llm.model", "value": "gpt-5" }`
//!   * `DELETE { "path": "llm.model" }`

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::{delete, get, put};
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::server::ApiState;

pub fn routes() -> Router<ApiState> {
    Router::new()
        .route("/config", get(get_config))
        .route("/config", put(set_config))
        .route("/config", delete(unset_config))
}

#[derive(Debug, Deserialize)]
struct SetRequest {
    path: String,
    value: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct UnsetRequest {
    path: String,
}

#[derive(Debug, Serialize)]
struct MutateResponse {
    path: String,
    written_to: String,
    /// Always `true`: the gateway keeps its in-memory snapshot and
    /// requires a restart for the new value to take effect.
    requires_restart: bool,
}

async fn get_config(State(state): State<ApiState>) -> Result<axum::Json<serde_json::Value>> {
    let value = serde_json::to_value(&*state.config)
        .map_err(|e| crate::GatewayError::Internal(e.to_string()))?;
    Ok(axum::Json(value))
}

async fn set_config(
    State(state): State<ApiState>,
    Json(req): Json<SetRequest>,
) -> Result<Json<MutateResponse>> {
    let target = state.config_path.as_ref().ok_or_else(|| {
        crate::GatewayError::BadRequest(
            "gateway was started without a config file; set AURA_CONFIG_PATH or pass --config \
             <path> so the mutation has a destination"
                .into(),
        )
    })?;

    let new_config = state
        .config
        .set_at_path(&req.path, req.value.clone())
        .map_err(|e| crate::GatewayError::BadRequest(e.to_string()))?;
    new_config
        .write_to_file(target)
        .await
        .map_err(|e| crate::GatewayError::Internal(e.to_string()))?;

    Ok(Json(MutateResponse {
        path: req.path,
        written_to: target.display().to_string(),
        requires_restart: true,
    }))
}

async fn unset_config(
    State(state): State<ApiState>,
    Json(req): Json<UnsetRequest>,
) -> Result<Json<MutateResponse>> {
    let target = state.config_path.as_ref().ok_or_else(|| {
        crate::GatewayError::BadRequest(
            "gateway was started without a config file; set AURA_CONFIG_PATH or pass --config \
             <path> so the mutation has a destination"
                .into(),
        )
    })?;

    let new_config = state
        .config
        .unset_at_path(&req.path)
        .map_err(|e| crate::GatewayError::BadRequest(e.to_string()))?;
    new_config
        .write_to_file(target)
        .await
        .map_err(|e| crate::GatewayError::Internal(e.to_string()))?;

    Ok(Json(MutateResponse {
        path: req.path,
        written_to: target.display().to_string(),
        requires_restart: true,
    }))
}
