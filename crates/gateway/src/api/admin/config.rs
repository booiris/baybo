//! `/v1/config` — read and mutate the loaded `BayboConfig`.
//!
//! `GET` returns a snapshot of the on-disk config (with secret fields
//! redacted by `baybo_config`'s serde impls). `PUT` / `DELETE` write
//! through to the same on-disk `baybo.json` that `baybo config set/unset`
//! targets, then trigger an in-process reload: a hot-updatable field
//! takes effect live; a non-hot field is persisted but needs a restart,
//! reported back via `requires_restart`.

use axum::Json;
use axum::extract::State;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::Result;
use crate::api::dto::{ErrorBody, MutateResponse, SetConfigRequest, UnsetConfigRequest};
use crate::reload::{ReloadError, ReloadOutcome};
use crate::server::AdminState;

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(get_config, set_config, unset_config))
        .routes(routes!(reload_config))
}

#[utoipa::path(
    get,
    path = "/config",
    tag = "config",
    responses(
        (
            status = 200,
            description = "Current in-memory config. Secret fields are redacted by `BayboConfig`'s serde impl.",
            body = serde_json::Value,
            content_type = "application/json",
        ),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Internal error", body = ErrorBody),
    )
)]
async fn get_config(State(state): State<AdminState>) -> Result<Json<serde_json::Value>> {
    // Read the current on-disk config (what a reload would apply), not
    // the boot snapshot — otherwise this lies after a hot-reload.
    let cfg = super::llm::read_config_for_dashboard(&state).await?;
    let value =
        serde_json::to_value(&cfg).map_err(|e| crate::GatewayError::Internal(e.to_string()))?;
    Ok(Json(value))
}

#[utoipa::path(
    put,
    path = "/config",
    tag = "config",
    request_body = SetConfigRequest,
    responses(
        (status = 200, description = "Config written to disk and applied in-process; `requires_restart` is true only when a non-hot field changed.", body = MutateResponse),
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
            "gateway was started without a config file; set BAYBO_CONFIG_PATH or pass --config \
             <path> so the mutation has a destination"
                .into(),
        )
    })?;

    // Build from the current on-disk config, not the boot snapshot, so
    // we don't clobber edits a prior hot-reload already applied.
    let current = super::llm::read_config_for_dashboard(&state).await?;
    let new_config = current
        .set_at_path(&req.path, req.value.clone())
        .map_err(|e| crate::GatewayError::BadRequest(e.to_string()))?;
    // Validate before persisting/applying: `apply_after_write` reloads
    // the file in-process, so an invalid value would otherwise take
    // effect live (e.g. a zero rate-limit bricking every request).
    new_config
        .validate()
        .map_err(|e| crate::GatewayError::BadRequest(e.to_string()))?;
    // Pre-flight the rebuild too, so a generic edit that breaks the
    // default model is rejected without dirtying the file.
    state.config_reloader.dry_run(&new_config).await?;
    new_config
        .write_to_file(target)
        .await
        .map_err(|e| crate::GatewayError::Internal(e.to_string()))?;

    Ok(Json(MutateResponse {
        path: req.path,
        written_to: target.display().to_string(),
        requires_restart: apply_after_write(&state).await?,
    }))
}

#[utoipa::path(
    delete,
    path = "/config",
    tag = "config",
    request_body = UnsetConfigRequest,
    responses(
        (status = 200, description = "Config entry removed on disk and applied in-process; `requires_restart` is true only when a non-hot field changed.", body = MutateResponse),
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
            "gateway was started without a config file; set BAYBO_CONFIG_PATH or pass --config \
             <path> so the mutation has a destination"
                .into(),
        )
    })?;

    let current = super::llm::read_config_for_dashboard(&state).await?;
    let new_config = current
        .unset_at_path(&req.path)
        .map_err(|e| crate::GatewayError::BadRequest(e.to_string()))?;
    new_config
        .validate()
        .map_err(|e| crate::GatewayError::BadRequest(e.to_string()))?;
    state.config_reloader.dry_run(&new_config).await?;
    new_config
        .write_to_file(target)
        .await
        .map_err(|e| crate::GatewayError::Internal(e.to_string()))?;

    Ok(Json(MutateResponse {
        path: req.path,
        written_to: target.display().to_string(),
        requires_restart: apply_after_write(&state).await?,
    }))
}

/// Apply the just-written config in-process. Returns whether a restart is
/// still required: a hot field is applied live (`false`); a non-hot field
/// is persisted but needs a restart, which the reloader reports as
/// `NotHotReloadable` (`true`, not an error). Any other reload failure
/// (invalid config, unbuildable default) propagates. Shared with the LLM
/// admin endpoints so a hot LLM edit that lands while a non-hot field is
/// already pending-restart on disk reports `requires_restart: true` instead
/// of a confusing 400.
pub(crate) async fn apply_after_write(state: &AdminState) -> Result<bool> {
    match state.config_reloader.reload().await {
        Ok(_) => Ok(false),
        Err(ReloadError::NotHotReloadable(_)) => Ok(true),
        Err(e) => Err(e.into()),
    }
}

#[utoipa::path(
    post,
    path = "/config/reload",
    tag = "config",
    responses(
        (status = 200, description = "Config re-read; hot-updatable changes applied in-process", body = ReloadOutcome),
        (status = 400, description = "Reload rejected — a non-hot field changed, the config is invalid, or the default model is unbuildable", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn reload_config(State(state): State<AdminState>) -> Result<Json<ReloadOutcome>> {
    let outcome = state.config_reloader.reload().await?;
    Ok(Json(outcome))
}
