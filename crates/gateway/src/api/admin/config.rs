//! `/v1/config` — read and mutate the loaded `AuraConfig`.
//!
//! `GET` returns a snapshot of the in-memory config (with secret fields
//! redacted by `aura_config`'s serde impls). `PUT` / `DELETE` write
//! through to the same on-disk `aura.json` that `aura config set/unset`
//! targets: we do not mutate the running process's `Arc<AuraConfig>` in
//! place, so callers must restart the gateway to pick the change up.

use axum::Json;
use axum::extract::State;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::Result;
use crate::api::dto::{ErrorBody, MutateResponse, SetConfigRequest, UnsetConfigRequest};
use crate::server::AdminState;

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new().routes(routes!(get_config, set_config, unset_config))
}

#[utoipa::path(
    get,
    path = "/config",
    tag = "config",
    responses(
        (
            status = 200,
            description = "Current in-memory config. Secret fields are redacted by `AuraConfig`'s serde impl.",
            body = serde_json::Value,
            content_type = "application/json",
        ),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Internal error", body = ErrorBody),
    )
)]
async fn get_config(State(state): State<AdminState>) -> Result<Json<serde_json::Value>> {
    let value = serde_json::to_value(&*state.config)
        .map_err(|e| crate::GatewayError::Internal(e.to_string()))?;
    Ok(Json(value))
}

#[utoipa::path(
    put,
    path = "/config",
    tag = "config",
    request_body = SetConfigRequest,
    responses(
        (status = 200, description = "Config updated on disk. Gateway restart required to pick up.", body = MutateResponse),
        (status = 400, description = "Invalid path or value", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Write failure", body = ErrorBody),
    )
)]
async fn set_config(
    State(state): State<AdminState>,
    Json(req): Json<SetConfigRequest>,
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

#[utoipa::path(
    delete,
    path = "/config",
    tag = "config",
    request_body = UnsetConfigRequest,
    responses(
        (status = 200, description = "Config entry removed on disk. Gateway restart required to pick up.", body = MutateResponse),
        (status = 400, description = "Invalid path", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Write failure", body = ErrorBody),
    )
)]
async fn unset_config(
    State(state): State<AdminState>,
    Json(req): Json<UnsetConfigRequest>,
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
