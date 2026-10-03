//! `/v1/llm` — snapshot of the configured LLM provider plus the
//! models-dashboard surface (`/v1/llm/models`, `/v1/llm/default`,
//! `/v1/llm/usage`, `/v1/llm/models/{name}/test`,
//! `/v1/llm/models/{name}/model-list`, `/v1/llm/models/{name}/catalog`).
//!
//! `GET /v1/llm` returns the *currently active* provider/model (whatever
//! the runtime loaded at startup). Edits land on disk and are hot-reloaded
//! in-process; `requires_restart` in the answer means the write reached the
//! file but not the running pool.
//!
//! **Which model an entry SERVES lives in two different places, on purpose.**
//! `PUT /llm/models/{name}` sets `model` — the entry's default, and the only
//! model whose overrides this API can edit. `PUT /llm/models/{name}/model-list`
//! sets the whole served SET, preserving each surviving id's overrides. Before
//! the latter existed, `model_list` could only grow (via `default_spec_mut`)
//! and nothing over HTTP could shrink it.

use axum::Json;
use axum::extract::{Path, Query, State};
use baybo_config::{BayboConfig, LlmEntry};
use baybo_cost::TimeRange;
use baybo_llm::credentials::{resolve_api_key, vault_api_key_name};
use baybo_llm::{CostHooks, LlmProviderConfig, LlmProviderRegistry};
use chrono::{Duration, Utc};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::Result;
use crate::api::dto::{
    ErrorBody, LlmCatalogModel, LlmCatalogResponse, LlmInfo, LlmModelEntry, LlmModelPricingDto,
    LlmModelTestResult, LlmModelUsage, LlmModelsResponse, LlmPricingOverrideDto, LlmUsageQuery,
    LlmUsageResponse, MutateResponse, SetDefaultLlmRequest, SetLlmModelListRequest,
    UpdateLlmModelRequest,
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
        .routes(routes!(set_model_list))
        .routes(routes!(get_catalog))
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
            "gateway was started without a config file; set BAYBO_CONFIG_PATH or pass --config \
             <path> so the mutation has a destination"
                .into(),
        )
    })?;

    // `context_window` / `supports_vision` / `pricing` describe A MODEL, and
    // this request has no way to say WHICH — they land wherever `model` leaves
    // the default pointing. A client that renders the entry and posts the form
    // back therefore moves the departing model's overrides onto its successor,
    // and gets a 200 for it. Refusing the combination is what makes that
    // unrepresentable; the caller changes the model, sees what the new one
    // actually resolves to, and then decides whether to override it.
    if req.model.is_some()
        && (req.context_window.is_some() || req.supports_vision.is_some() || req.pricing.is_some())
    {
        return Err(GatewayError::BadRequest(
            "`model` cannot be changed in the same request as `context_window`, \
             `supports_vision` or `pricing`: those describe a model, and this request cannot say \
             which one they belong to. Send the model change on its own first."
                .into(),
        ));
    }

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
    // The three below are facts about a model, so they land on the default
    // model's `model_list` spec. Overrides for the entry's *other* models
    // are config-file edits — this endpoint only addresses the default.
    if let Some(vision) = req.supports_vision {
        entry.default_spec_mut().supports_vision = vision;
    }
    if let Some(ctx) = req.context_window {
        if let Some(0) = ctx {
            return Err(GatewayError::BadRequest(
                "context_window must be > 0 (omit field or pass null to clear override)".into(),
            ));
        }
        entry.default_spec_mut().context_window = ctx;
    }
    if let Some(p) = req.pricing {
        entry.default_spec_mut().pricing = p.map(Into::into);
    }

    current
        .validate()
        .map_err(|e| GatewayError::BadRequest(e.to_string()))?;

    // Stage the API-key change in the vault *before* the pre-flight build:
    // every provider resolves its credential at client construction, so a dry
    // run that still saw the old key would validate a build nobody asked for.
    // An empty string is a CLEAR — the key is deleted, not left in place.
    let staged = stage_api_key(&state, &name, req.api_key.as_deref()).await?;

    // Pre-flight before persisting, so an unbuildable edit is rejected
    // without dirtying the file (a later SIGHUP would otherwise re-read and
    // silently drop it). With the new key already staged, this validates
    // the real post-edit build.
    //
    // Both fallible steps below undo the vault first. Without that, a rejected
    // pre-flight would answer 400 with the config untouched and the secret
    // already gone — and since the clear IS the only way to remove a key,
    // there would be nothing left to put back.
    if let Err(e) = state.config_reloader.dry_run(&current).await {
        staged.restore(&state).await;
        return Err(e.into());
    }
    if let Err(e) = current.write_to_file(target).await {
        staged.restore(&state).await;
        return Err(GatewayError::Internal(e.to_string()));
    }

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
    // The probe exercises the entry's default model, so it wants that
    // model's overrides.
    let probe_spec = entry
        .spec_for(&entry.model)
        .unwrap_or_else(|| baybo_config::LlmModelSpec::bare(entry.model.clone()));
    let provider_cfg = LlmProviderConfig {
        provider: entry.provider.clone(),
        api_key,
        base_url: entry.base_url.clone(),
        model: entry.model.clone(),
        supports_vision: probe_spec.supports_vision,
        context_window: probe_spec.context_window,
        pricing: probe_spec.pricing,
        reasoning_effort: entry.reasoning_effort.clone(),
        vault: Some(state.secret_vault.clone()),
        proxy: cfg
            .proxy
            .as_ref()
            .map(|p| baybo_security::http::ProxySettings {
                url: p.url.clone(),
                no_proxy: p.no_proxy.clone(),
            }),
    };

    let client = match registry.create_client(&provider_cfg, None, CostHooks::passthrough()) {
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
    path = "/llm/models/{name}/model-list",
    tag = "llm",
    params(
        ("name" = String, Path, description = "Entry name (matches `llm[*].name`)"),
    ),
    request_body = SetLlmModelListRequest,
    responses(
        (status = 200, description = "Model list replaced and hot-reloaded in-process.", body = MutateResponse),
        (status = 400, description = "Empty id, duplicate, or the default model missing", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Entry not found", body = ErrorBody),
        (status = 500, description = "Write failure", body = ErrorBody),
    )
)]
async fn set_model_list(
    State(state): State<AdminState>,
    Path(name): Path<String>,
    Json(req): Json<SetLlmModelListRequest>,
) -> Result<Json<MutateResponse>> {
    let target = state.config_path.as_ref().ok_or_else(|| {
        GatewayError::BadRequest(
            "gateway was started without a config file; set BAYBO_CONFIG_PATH or pass --config \
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

    let mut models: Vec<String> = Vec::with_capacity(req.models.len());
    for raw in &req.models {
        let model = raw.trim();
        if model.is_empty() {
            return Err(GatewayError::BadRequest(
                "model ids must be non-empty".into(),
            ));
        }
        // A duplicate would make `spec_for` ambiguous — it returns the first
        // match, so the second copy's overrides would be silently inert.
        if models.iter().any(|m| m == model) {
            return Err(GatewayError::BadRequest(format!(
                "model {model:?} is listed twice"
            )));
        }
        models.push(model.to_string());
    }

    // The default model has to stay in its own entry's list. `LlmEntry::models`
    // prepends it when absent, so omitting it would not actually remove it —
    // the write would report a set the entry does not have.
    if !models.iter().any(|m| m == entry.model.as_str()) {
        return Err(GatewayError::BadRequest(format!(
            "the entry's default model {:?} must stay in its model list; change `model` first if \
             you meant to replace it",
            entry.model
        )));
    }

    // Carry each surviving id's overrides across. Only an id that was not
    // there gets a bare spec, so a caller may send plain ids without having to
    // know — or echo back — what the operator configured.
    let previous = std::mem::take(&mut entry.model_list);
    entry.model_list = models
        .into_iter()
        .map(|model| {
            previous
                .iter()
                .find(|s| s.model == model)
                .cloned()
                .unwrap_or_else(|| baybo_config::LlmModelSpec::bare(model))
        })
        .collect();

    // `lite_model` pointing outside the new list is caught here — the
    // validator owns that rule and its message already says how to fix it.
    current
        .validate()
        .map_err(|e| GatewayError::BadRequest(e.to_string()))?;
    state.config_reloader.dry_run(&current).await?;
    current
        .write_to_file(target)
        .await
        .map_err(|e| GatewayError::Internal(e.to_string()))?;

    let requires_restart = super::config::apply_after_write(&state).await?;

    Ok(Json(MutateResponse {
        path: format!("llm[{name}].model_list"),
        written_to: target.display().to_string(),
        requires_restart,
    }))
}

#[utoipa::path(
    get,
    path = "/llm/models/{name}/catalog",
    tag = "llm",
    params(
        ("name" = String, Path, description = "Entry name (matches `llm[*].name`)"),
    ),
    responses(
        (status = 200, description = "The provider's live model catalog", body = LlmCatalogResponse),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 404, description = "Entry not found", body = ErrorBody),
        (status = 502, description = "The provider's catalog could not be read", body = ErrorBody),
    )
)]
async fn get_catalog(
    State(state): State<AdminState>,
    Path(name): Path<String>,
) -> Result<Json<LlmCatalogResponse>> {
    let cfg = read_config_for_dashboard(&state).await?;
    let entry = cfg
        .llm
        .iter()
        .find(|e| e.name == name)
        .ok_or_else(|| GatewayError::NotFound(format!("llm entry {name:?}")))?
        .clone();

    let registry = LlmProviderRegistry::with_default_providers();
    let provider_cfg = LlmProviderConfig {
        provider: entry.provider.clone(),
        api_key: resolve_api_key(
            entry.name.as_str(),
            &entry.provider,
            entry.api_key_env.as_deref(),
            Some(state.secret_vault.as_ref()),
        )
        .await,
        base_url: entry.base_url.clone(),
        // Listing a catalog names no model, and an entry mid-setup can have an
        // empty one — the client still has to build.
        model: if entry.model.is_empty() {
            "unused".into()
        } else {
            entry.model.clone()
        },
        // Catalog listing only — no billing, no completion — so the entry's
        // per-model overrides have nothing to say here.
        supports_vision: None,
        context_window: None,
        pricing: None,
        reasoning_effort: entry.reasoning_effort.clone(),
        vault: Some(state.secret_vault.clone()),
        proxy: cfg
            .proxy
            .as_ref()
            .map(|p| baybo_security::http::ProxySettings {
                url: p.url.clone(),
                no_proxy: p.no_proxy.clone(),
            }),
    };

    let live = registry
        .list_live_models(&provider_cfg)
        .await
        // The failure is the provider's or the credential's, not this
        // gateway's, and the operator's fix is on the entry.
        .map_err(|e| GatewayError::BadRequest(format!("live model discovery: {e}")))?;

    let configured = entry.models();
    Ok(Json(LlmCatalogResponse {
        items: live
            .into_iter()
            .map(|m| LlmCatalogModel {
                configured: configured.iter().any(|s| s.model == m.id),
                id: m.id,
                display_name: m.display_name,
                context_window: m.context_window,
                supports_vision: m.supports_vision,
            })
            .collect(),
    }))
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
            let mut cost = baybo_model::MicroUsd::ZERO;
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

/// A vault edit made ahead of the pre-flight that can still reject it, plus
/// what it takes to put the vault back.
///
/// The ordering is forced: providers resolve their credential when the client
/// is constructed, so `dry_run` has to see the intended key. That leaves the
/// window this type closes — a 400 after the secret already moved.
struct StagedApiKey {
    /// `None` when the request carried no `api_key` at all, i.e. nothing to
    /// undo. Otherwise the vault name and whatever was there before.
    undo: Option<(String, Option<Vec<u8>>)>,
}

impl StagedApiKey {
    const NONE: Self = Self { undo: None };

    /// Put the vault back the way it was. Best-effort and never fatal: the
    /// caller is already returning an error, and a failed restore must not
    /// replace that error's message with this one — but it does deserve a loud
    /// log, because it is the one path that can strand a credential.
    async fn restore(self, state: &AdminState) {
        let Some((vault_name, previous)) = self.undo else {
            return;
        };
        let result = match &previous {
            Some(bytes) => state.secret_vault.store_secret(&vault_name, bytes).await,
            None => state.secret_vault.delete_secret(&vault_name).await,
        };
        if let Err(e) = result {
            tracing::error!(
                error = %e,
                secret = %vault_name,
                restoring = if previous.is_some() { "previous key" } else { "absence" },
                "failed to roll back a staged api key after a rejected edit; the vault may now \
                 disagree with the config on disk"
            );
        }
    }
}

/// Apply the request's `api_key` to the vault, returning the undo.
///
/// Three states, and they are the request's, not this function's invention:
/// absent leaves the vault alone, `""` deletes the stored key, and a value
/// replaces it.
async fn stage_api_key(
    state: &AdminState,
    entry: &str,
    api_key: Option<&str>,
) -> GatewayResult<StagedApiKey> {
    let Some(api_key) = api_key else {
        return Ok(StagedApiKey::NONE);
    };
    let vault_name = vault_api_key_name(entry);
    let previous = state
        .secret_vault
        .get_secret(&vault_name)
        .await
        .map_err(|e| GatewayError::Internal(format!("vault read failed: {e}")))?
        .map(|v| v.as_bytes().to_vec());

    if api_key.is_empty() {
        state
            .secret_vault
            .delete_secret(&vault_name)
            .await
            .map_err(|e| GatewayError::Internal(format!("vault delete failed: {e}")))?;
        tracing::info!(entry = %entry, "api key cleared from the vault");
    } else {
        state
            .secret_vault
            .store_secret(&vault_name, api_key.as_bytes())
            .await
            .map_err(|e| GatewayError::Internal(format!("vault write failed: {e}")))?;
    }
    Ok(StagedApiKey {
        undo: Some((vault_name, previous)),
    })
}

/// Read the on-disk config for dashboard reads/writes. Falls back to
/// the cached `state.config` snapshot when no `config_path` was set
/// (dev mode with implicit defaults) — mutation endpoints still gate
/// on `config_path` separately.
pub(crate) async fn read_config_for_dashboard(state: &AdminState) -> GatewayResult<BayboConfig> {
    match state.config_path.as_ref() {
        Some(path) if path.exists() => BayboConfig::load_from_file(path)
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
    cfg: &BayboConfig,
    entry: &LlmEntry,
) -> LlmModelEntry {
    let caps = baybo_llm::openrouter::capabilities_for(&entry.provider, &entry.model);
    let factory_pricing =
        baybo_llm::openrouter::pricing_for(&entry.provider, &entry.model).unwrap_or_default();
    let defaults = baybo_llm::factory_defaults_for(&entry.provider);

    // The dashboard's "effective" columns describe the entry's DEFAULT
    // model, so they layer that model's own spec — not an entry-wide one.
    let default_spec = entry
        .spec_for(&entry.model)
        .unwrap_or_else(|| baybo_config::LlmModelSpec::bare(entry.model.clone()));

    let effective_context_window = default_spec
        .context_window
        .or_else(|| caps.and_then(|c| c.context_window))
        .unwrap_or(defaults.context_window);
    let effective_supports_vision = default_spec
        .supports_vision
        .or_else(|| caps.and_then(|c| c.supports_vision))
        .unwrap_or(defaults.supports_vision);

    let mut effective_pricing = LlmModelPricingDto {
        input_per_1m_tokens: factory_pricing.input_per_1m_tokens,
        output_per_1m_tokens: factory_pricing.output_per_1m_tokens,
        cached_input_per_1m_tokens: factory_pricing.cached_input_per_1m_tokens,
        cache_write_per_1m_tokens: factory_pricing.cache_write_per_1m_tokens,
    };
    if let Some(p) = default_spec.pricing {
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
    // Separate from `configured`, which an env var also satisfies: only a
    // stored key can be deleted over HTTP, so only a stored key may be
    // offered for deletion.
    let api_key_in_vault = state
        .secret_vault
        .get_secret(&vault_api_key_name(entry.name.as_str()))
        .await
        .ok()
        .flatten()
        .is_some();

    LlmModelEntry {
        name: entry.name.to_string(),
        provider: entry.provider.clone(),
        model: entry.model.clone(),
        model_list: entry.models().into_iter().map(Into::into).collect(),
        supports_vision_override: default_spec.supports_vision,
        context_window_override: default_spec.context_window,
        pricing_override: default_spec.pricing.map(LlmPricingOverrideDto::from),
        lite_model: entry.lite_model.clone(),
        base_url: entry.base_url.clone(),
        api_key_env: entry.api_key_env.clone(),
        api_key_configured,
        api_key_in_vault,
        reasoning_effort: entry.reasoning_effort.clone(),
        available_efforts: baybo_llm::providers::effort_wire_for_provider(&entry.provider)
            .levels()
            .iter()
            .map(|level| level.to_string())
            .collect(),
        is_default: cfg.default_llm == entry.name,
        effective_context_window,
        effective_supports_vision,
        effective_pricing,
    }
}
