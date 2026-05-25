//! `/v1/llm` — snapshot of the configured LLM provider plus the
//! models-dashboard surface (`/v1/llm/models`, `/v1/llm/default`,
//! `/v1/llm/usage`, `/v1/llm/models/{name}/test`).
//!
//! `GET /v1/llm` returns the *currently active* provider/model (whatever
//! the runtime loaded at startup). Edits land on disk and require a
//! gateway restart to take effect — same contract as `PUT /v1/config`.


use aura_config::{AuraConfig, LlmEntry};
use aura_cost::TimeRange;
use aura_llm::credentials::{resolve_api_key, vault_api_key_name};
use aura_llm::{LlmBilling, LlmProviderConfig, LlmProviderRegistry};
use axum::Json;
use axum::extract::{Path, Query, State};
use chrono::{Duration, Utc};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::Result;
use crate::api::dto::{
    ErrorBody, LlmInfo, LlmModelEntry, LlmModelPricingDto, LlmModelTestResult, LlmModelUsage,
    LlmModelsResponse, LlmPricingOverrideDto, LlmUsageQuery, LlmUsageResponse, MutateResponse,
    SetDefaultLlmRequest, UpdateLlmModelRequest,
};
use crate::server::AdminState;
use crate::{GatewayError, Result as GatewayResult};

/// Default lookback window for `GET /v1/llm/usage` when neither
/// `since` nor `until` is supplied. Matches `/v1/analytics`.
const DEFAULT_USAGE_DAYS: i64 = 30;

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(get_llm))
        .routes(routes!(list_models))
        .routes(routes!(update_model))
        .routes(routes!(test_model))
        .routes(routes!(set_default))
        .routes(routes!(get_usage))
}

#[utoipa::path(
    get,
    path = "/llm",
    tag = "llm",
    responses(
        (status = 200, description = "Currently active LLM provider", body = LlmInfo),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn get_llm(State(state): State<AdminState>) -> Result<Json<LlmInfo>> {
    // Read-through: resolve the current default from the live pool so
    // this reflects a config hot-reload, not the boot-time client.
    let client = state.llm_pool.read().default_client();
    let info = client.model_info();
    Ok(Json(LlmInfo {
        model_id: info.id.clone(),
        provider: info.provider.clone(),
    }))
}

#[utoipa::path(
    get,
    path = "/llm/models",
    tag = "llm",
    responses(
        (status = 200, description = "Configured LLM entries with their effective settings", body = LlmModelsResponse),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn list_models(State(state): State<AdminState>) -> Result<Json<LlmModelsResponse>> {
    let cfg = read_config_for_dashboard(&state).await?;

    let mut items = Vec::with_capacity(cfg.llm.len());
    for entry in &cfg.llm {
        items.push(build_model_entry(&state, &cfg, entry).await);
    }
    Ok(Json(LlmModelsResponse {
        default_name: cfg.default_llm.to_string(),
        items,
    }))
}

#[utoipa::path(
    put,
    path = "/llm/models/{name}",
    tag = "llm",
    params(
        ("name" = String, Path, description = "Entry name (matches `llm[*].name`)"),
    ),
    request_body = UpdateLlmModelRequest,
    responses(
        (status = 200, description = "Entry updated and hot-reloaded in-process (no restart needed).", body = MutateResponse),
        (status = 400, description = "Invalid update", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Entry not found", body = ErrorBody),
        (status = 500, description = "Write failure", body = ErrorBody),
    )
)]
async fn update_model(
    State(state): State<AdminState>,
    Path(name): Path<String>,
    Json(req): Json<UpdateLlmModelRequest>,
) -> Result<Json<MutateResponse>> {
    let target = state.config_path.as_ref().ok_or_else(|| {
        GatewayError::BadRequest(
            "gateway was started without a config file; set AURA_CONFIG_PATH or pass --config \
             <path> so the mutation has a destination"
                .into(),
        )
    })?;

    let mut current = read_config_for_dashboard(&state).await?;
    let entry = current
        .llm
        .iter_mut()
        .find(|e| e.name == name)
        .ok_or_else(|| GatewayError::NotFound(format!("llm entry {name:?}")))?;

    if let Some(provider) = req.provider {
        entry.provider = provider;
    }
    if let Some(model) = req.model {
        entry.model = model;
    }
    if let Some(base_url) = req.base_url {
        entry.base_url = base_url.filter(|s| !s.is_empty());
    }
    if let Some(env) = req.api_key_env {
        entry.api_key_env = env.filter(|s| !s.is_empty());
    }
    if let Some(effort) = req.reasoning_effort {
        entry.reasoning_effort = effort.filter(|s| !s.is_empty());
    }
    if let Some(vision) = req.supports_vision {
        entry.supports_vision = vision;
    }
    if let Some(ctx) = req.context_window {
        if let Some(0) = ctx {
            return Err(GatewayError::BadRequest(
                "context_window must be > 0 (omit field or pass null to clear override)".into(),
            ));
        }
        entry.context_window = ctx;
    }
    if let Some(p) = req.pricing {
        entry.pricing = p.map(Into::into);
    }

    current
        .validate()
        .map_err(|e| GatewayError::BadRequest(e.to_string()))?;

    // Stage a rotated API key in the vault *before* the pre-flight build:
    // every provider requires the key at client construction, so dry-run
    // (and the rebuild) must resolve the new credential, not the old. Empty
    // string = clear, a no-op here — there's no per-key delete on this
    // path, so the prior secret stays until a fresh value is set.
    if let Some(api_key) = req.api_key.as_deref() {
        if api_key.is_empty() {
            tracing::info!(
                entry = %name,
                "api_key clear requested; this path has no per-key vault delete, leaving the prior \
                 secret in place (rotate by setting a fresh non-empty value)"
            );
        } else {
            state
                .secret_vault
                .store_secret(&vault_api_key_name(&name), api_key.as_bytes())
                .await
                .map_err(|e| GatewayError::Internal(format!("vault write failed: {e}")))?;
        }
    }

    // Pre-flight before persisting, so an unbuildable edit is rejected
    // without dirtying the file (a later SIGHUP would otherwise re-read and
    // silently drop it). With the new key already staged, this validates
    // the real post-edit build.
    state.config_reloader.dry_run(&current).await?;
    current
        .write_to_file(target)
        .await
        .map_err(|e| GatewayError::Internal(e.to_string()))?;

    // Apply in-process. `reload` always rebuilds the LLM pool, so a vault
    // key rotation (invisible in the config diff) takes effect too. If a
    // non-hot field is already pending-restart on disk (a prior `PUT
    // /v1/config` edit), the reload reports `NotHotReloadable` — the LLM
    // edit is still persisted, so surface restart-pending rather than 400.
    // A genuine rebuild failure (unbuildable default) still propagates.
    let requires_restart = super::config::apply_after_write(&state).await?;

    Ok(Json(MutateResponse {
        path: format!("llm[{name}]"),
        written_to: target.display().to_string(),
        requires_restart,
    }))
}

#[utoipa::path(
    post,
    path = "/llm/models/{name}/test",
    tag = "llm",
    params(
        ("name" = String, Path, description = "Entry name (matches `llm[*].name`)"),
    ),
    responses(
        (status = 200, description = "Probe result — `ok: false` carries the provider's error verbatim", body = LlmModelTestResult),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Entry not found", body = ErrorBody),
    )
)]
async fn test_model(
    State(state): State<AdminState>,
    Path(name): Path<String>,
) -> Result<Json<LlmModelTestResult>> {
    let cfg = read_config_for_dashboard(&state).await?;
    let entry = cfg
        .llm
        .iter()
        .find(|e| e.name == name)
        .ok_or_else(|| GatewayError::NotFound(format!("llm entry {name:?}")))?
        .clone();

    let registry = LlmProviderRegistry::with_default_providers();
    let api_key = resolve_api_key(
        entry.name.as_str(),
        &entry.provider,
        entry.api_key_env.as_deref(),
        Some(state.secret_vault.as_ref()),
    )
    .await;
    let provider_cfg = LlmProviderConfig {
        provider: entry.provider.clone(),
        api_key,
        base_url: entry.base_url.clone(),
        model: entry.model.clone(),
        supports_vision: entry.supports_vision,
        context_window: entry.context_window,
        pricing: entry.pricing,
        reasoning_effort: entry.reasoning_effort.clone(),
        vault: Some(state.secret_vault.clone()),
    };

    let client = match registry.create_client(&provider_cfg, None, LlmBilling::passthrough()) {
        Ok(c) => c,
        Err(e) => {
            return Ok(Json(LlmModelTestResult {
                ok: false,
                error: Some(format!("client setup: {e}")),
                latency_ms: None,
                input_tokens: None,
                output_tokens: None,
                provider: entry.provider,
                model: entry.model,
            }));
        }
    };

    match client.probe().await {
        Ok(report) => Ok(Json(LlmModelTestResult {
            ok: true,
            error: None,
            latency_ms: Some(report.latency_ms),
            input_tokens: Some(report.tokens.input_tokens),
            output_tokens: Some(report.tokens.output_tokens),
            provider: report.provider,
            model: report.model,
        })),
        Err(e) => Ok(Json(LlmModelTestResult {
            ok: false,
            error: Some(e.to_string()),
            latency_ms: None,
            input_tokens: None,
            output_tokens: None,
            provider: entry.provider,
            model: entry.model,
        })),
    }
}

#[utoipa::path(
    put,
    path = "/llm/default",
    tag = "llm",
    request_body = SetDefaultLlmRequest,
    responses(
        (status = 200, description = "`default-llm` updated and hot-reloaded in-process (no restart needed).", body = MutateResponse),
        (status = 400, description = "Name does not match any entry", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Write failure", body = ErrorBody),
    )
)]
async fn set_default(
    State(state): State<AdminState>,
    Json(req): Json<SetDefaultLlmRequest>,
) -> Result<Json<MutateResponse>> {
    let target = state.config_path.as_ref().ok_or_else(|| {
        GatewayError::BadRequest(
            "gateway was started without a config file; cannot persist default-llm".into(),
        )
    })?;

    let mut current = read_config_for_dashboard(&state).await?;
    if !current.llm.iter().any(|e| e.name == req.name) {
        return Err(GatewayError::BadRequest(format!(
            "no LLM entry named {:?}",
            req.name
        )));
    }
    current.default_llm = req.name.clone().into();
    current
        .validate()
        .map_err(|e| GatewayError::BadRequest(e.to_string()))?;
    // Pre-flight: reject a default that can't be built before writing it.
    state.config_reloader.dry_run(&current).await?;
    current
        .write_to_file(target)
        .await
        .map_err(|e| GatewayError::Internal(e.to_string()))?;

    // Restart-pending (not 400) if a non-hot field is already pending on
    // disk; the default-llm edit is persisted regardless. See `update_model`.
    let requires_restart = super::config::apply_after_write(&state).await?;

    Ok(Json(MutateResponse {
        path: "default-llm".into(),
        written_to: target.display().to_string(),
        requires_restart,
    }))
}

#[utoipa::path(
    get,
    path = "/llm/usage",
    tag = "llm",
    params(LlmUsageQuery),
    responses(
        (status = 200, description = "Per-entry usage aggregates over the time range", body = LlmUsageResponse),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn get_usage(
    State(state): State<AdminState>,
    Query(q): Query<LlmUsageQuery>,
) -> Result<Json<LlmUsageResponse>> {
    let until = q.until.unwrap_or_else(Utc::now);
    let since = q
        .since
        .unwrap_or(until - Duration::days(DEFAULT_USAGE_DAYS));
    if since >= until {
        return Err(GatewayError::BadRequest(
            "since must be strictly less than until".into(),
        ));
    }
    let records = state
        .cost_store
        .query_records_in_range(TimeRange {
            from: since,
            to: until,
        })
        .await
        .map_err(|e| GatewayError::Trace(e.to_string()))?;

    let cfg = read_config_for_dashboard(&state).await?;

    // Aggregate by entry name. Multiple entries can target the same
    // model id — split cost records across them by name? In practice
    // cost_records carry the model id, not entry name. Two entries with
    // the same model id would attribute identically, which is the
    // historically correct behaviour (the runtime never knew which
    // entry produced a span). Group by entry; if two entries share a
    // model, they both surface the same totals — the dashboard then
    // makes the disambiguation visible to the operator.
    let items = cfg
        .llm
        .iter()
        .map(|entry| {
            let matching = records.iter().filter(|r| r.model == entry.model);
            let mut input_tokens = 0usize;
            let mut output_tokens = 0usize;
            let mut cached = 0usize;
            let mut cache_create = 0usize;
            let mut cost = aura_model::MicroUsd::ZERO;
            let mut count = 0usize;
            for r in matching {
                input_tokens += r.input_tokens;
                output_tokens += r.output_tokens;
                cached += r.cached_input_tokens;
                cache_create += r.cache_creation_input_tokens;
                cost += r.cost_usd;
                count += 1;
            }
            LlmModelUsage {
                name: entry.name.to_string(),
                model: entry.model.clone(),
                call_count: count,
                input_tokens,
                output_tokens,
                cached_input_tokens: cached,
                cache_creation_input_tokens: cache_create,
                cost_micro_usd: cost,
            }
        })
        .collect();

    Ok(Json(LlmUsageResponse {
        since,
        until,
        items,
    }))
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Read the on-disk config for dashboard reads/writes. Falls back to
/// the cached `state.config` snapshot when no `config_path` was set
/// (dev mode with implicit defaults) — mutation endpoints still gate
/// on `config_path` separately.
pub(crate) async fn read_config_for_dashboard(state: &AdminState) -> GatewayResult<AuraConfig> {
    match state.config_path.as_ref() {
        Some(path) if path.exists() => AuraConfig::load_from_file(path)
            .await
            .map_err(|e| GatewayError::Internal(format!("config reload: {e}"))),
        _ => Ok((*state.config).clone()),
    }
}

/// Synthesize the dashboard view of one config entry.
///
/// "Effective" values come from layering the entry's overrides on top
/// of the OpenRouter snapshot capabilities + factory defaults. The
/// derivation deliberately doesn't go through `LlmProviderRegistry::
/// build_client` because that path requires a valid API key, and the
/// dashboard must render entries that aren't yet wired up.
async fn build_model_entry(
    state: &AdminState,
    cfg: &AuraConfig,
    entry: &LlmEntry,
) -> LlmModelEntry {
    let caps = aura_llm::openrouter::capabilities_for(&entry.provider, &entry.model);
    let factory_pricing =
        aura_llm::openrouter::pricing_for(&entry.provider, &entry.model).unwrap_or_default();
    let defaults = aura_llm::factory_defaults_for(&entry.provider);

    let effective_context_window = entry
        .context_window
        .or_else(|| caps.and_then(|c| c.context_window))
        .unwrap_or(defaults.context_window);
    let effective_supports_vision = entry
        .supports_vision
        .or_else(|| caps.and_then(|c| c.supports_vision))
        .unwrap_or(defaults.supports_vision);

    let mut effective_pricing = LlmModelPricingDto {
        input_per_1m_tokens: factory_pricing.input_per_1m_tokens,
        output_per_1m_tokens: factory_pricing.output_per_1m_tokens,
        cached_input_per_1m_tokens: factory_pricing.cached_input_per_1m_tokens,
        cache_write_per_1m_tokens: factory_pricing.cache_write_per_1m_tokens,
    };
    if let Some(p) = entry.pricing {
        if let Some(v) = p.input_per_1m_tokens {
            effective_pricing.input_per_1m_tokens = v;
        }
        if let Some(v) = p.output_per_1m_tokens {
            effective_pricing.output_per_1m_tokens = v;
        }
        if p.cached_input_per_1m_tokens.is_some() {
            effective_pricing.cached_input_per_1m_tokens = p.cached_input_per_1m_tokens;
        }
        if p.cache_write_per_1m_tokens.is_some() {
            effective_pricing.cache_write_per_1m_tokens = p.cache_write_per_1m_tokens;
        }
    }

    let api_key_configured = resolve_api_key(
        entry.name.as_str(),
        &entry.provider,
        entry.api_key_env.as_deref(),
        Some(state.secret_vault.as_ref()),
    )
    .await
    .is_some();

    LlmModelEntry {
        name: entry.name.to_string(),
        provider: entry.provider.clone(),
        model: entry.model.clone(),
        base_url: entry.base_url.clone(),
        api_key_env: entry.api_key_env.clone(),
        api_key_configured,
        reasoning_effort: entry.reasoning_effort.clone(),
        is_default: cfg.default_llm == entry.name,
        supports_vision_override: entry.supports_vision,
        context_window_override: entry.context_window,
        pricing_override: entry.pricing.map(LlmPricingOverrideDto::from),
        effective_context_window,
        effective_supports_vision,
        effective_pricing,
    }
}
