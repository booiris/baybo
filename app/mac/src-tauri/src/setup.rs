//! Bespoke first-run setup commands (docs/mac-app.md §5). The webview drives a
//! native multi-screen wizard through these Tauri commands; they reuse the
//! pure `aura-setup` / `aura-llm` primitives directly — no TTY `Prompter`.
//!
//! Cross-command state (the bootstrapped `SetupContext` + the pending entry)
//! lives in `AppState::draft`. Guard discipline: never hold the `parking_lot`
//! lock across an `.await` (extract owned data, drop the guard, then await).

use std::path::PathBuf;
use std::sync::Arc;

use aura_config::{AuraConfig, LlmEntry};
use aura_security::SecretVault;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::AppState;

/// Wizard state held between command calls (created by `setup_status`).
pub(crate) struct SetupDraft {
    config_path: PathBuf,
    config: AuraConfig,
    vault: Arc<SecretVault>,
    pending: Option<PendingEntry>,
}

struct PendingEntry {
    entry_name: String,
    base_url: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupStatus {
    configured: bool,
    workspace_path: String,
    git_available: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    id: String,
    default_base_url: Option<String>,
    default_api_key_env: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryRef {
    entry_name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    id: String,
    display_name: Option<String>,
    context_window: Option<usize>,
}

#[derive(Serialize)]
pub struct Done {
    ok: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OauthOutcome {
    entry_name: String,
    email: Option<String>,
    plan: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitApiKey {
    provider: String,
    base_url: Option<String>,
    api_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListModels {
    provider: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FinishSetup {
    provider: String,
    entry_name: String,
    base_url: Option<String>,
    model: String,
    reasoning_effort: Option<String>,
}

/// Bootstrap the app-owned workspace (idempotent), stash the draft for the
/// rest of the wizard, and report whether the install is already configured.
#[tauri::command]
pub async fn setup_status(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<SetupStatus, String> {
    let root = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app_data_dir: {e}"))?;
    let ctx = aura_setup::bootstrap_workspace_if_needed(root.clone())
        .await
        .map_err(|e| format!("bootstrap: {e}"))?;
    let configured = crate::is_configured(&ctx.config);
    *state.draft.lock() = Some(SetupDraft {
        config_path: ctx.config_path.clone(),
        config: ctx.config.clone(),
        vault: ctx.vault.clone(),
        pending: None,
    });
    Ok(SetupStatus {
        configured,
        workspace_path: root.to_string_lossy().into_owned(),
        git_available: git_available(),
    })
}

/// The built-in LLM provider catalog, with prefilled base-URL / env hints.
#[tauri::command]
pub fn list_providers() -> Vec<ProviderInfo> {
    let registry = aura_llm::LlmProviderRegistry::with_default_providers();
    registry
        .provider_names()
        .into_iter()
        .map(|id| ProviderInfo {
            id: id.to_string(),
            default_base_url: aura_llm::default_base_url_for_provider(id).map(str::to_string),
            default_api_key_env: aura_llm::default_api_key_env_for_provider(id).map(str::to_string),
        })
        .collect()
}

/// Store an API key in the encrypted vault under the entry's canonical name and
/// record the pending entry. Does NOT write `aura.json` (that's `finish_setup`).
#[tauri::command]
pub async fn submit_api_key(
    state: State<'_, AppState>,
    req: SubmitApiKey,
) -> Result<EntryRef, String> {
    if req.api_key.trim().is_empty() {
        return Err("api key must be non-empty".into());
    }
    let (vault, entry_name) = {
        let guard = state.draft.lock();
        let draft = guard
            .as_ref()
            .ok_or("setup not initialized; call setup_status first")?;
        let name = unique_entry_name(&req.provider, &draft.config.llm);
        (draft.vault.clone(), name)
    };
    vault
        .store_secret(
            &aura_llm::credentials::vault_api_key_name(&entry_name),
            req.api_key.as_bytes(),
        )
        .await
        .map_err(|e| format!("vault write: {e}"))?;
    if let Some(draft) = state.draft.lock().as_mut() {
        draft.pending = Some(PendingEntry {
            entry_name: entry_name.clone(),
            base_url: req.base_url.clone().filter(|s| !s.trim().is_empty()),
        });
    }
    Ok(EntryRef { entry_name })
}

/// Live model discovery for the picker. Resolves the key just stored in the
/// vault; falls back to the provider default base URL.
#[tauri::command]
pub async fn list_models(
    state: State<'_, AppState>,
    req: ListModels,
) -> Result<Vec<ModelInfo>, String> {
    let (vault, entry_name, base_url) = {
        let guard = state.draft.lock();
        let draft = guard.as_ref().ok_or("setup not initialized")?;
        let pending = draft.pending.as_ref();
        let entry_name = pending
            .map(|p| p.entry_name.clone())
            .unwrap_or_else(|| req.provider.clone());
        let base_url = pending.and_then(|p| p.base_url.clone());
        (draft.vault.clone(), entry_name, base_url)
    };
    let api_key =
        aura_llm::credentials::resolve_api_key(&entry_name, &req.provider, None, Some(vault.as_ref()))
            .await;
    let registry = aura_llm::LlmProviderRegistry::with_default_providers();
    let cfg = aura_llm::LlmProviderConfig {
        provider: req.provider.clone(),
        api_key,
        base_url: base_url
            .or_else(|| aura_llm::default_base_url_for_provider(&req.provider).map(str::to_string)),
        model: "unused".to_string(),
        supports_vision: None,
        context_window: None,
        pricing: None,
        reasoning_effort: None,
        vault: Some(vault),
        proxy: None,
    };
    let models = registry
        .list_live_models(&cfg)
        .await
        .map_err(|e| format!("model discovery: {e}"))?;
    Ok(models
        .into_iter()
        .map(|m| ModelInfo {
            id: m.id,
            display_name: m.display_name,
            context_window: m.context_window,
        })
        .collect())
}

/// Assemble the `LlmEntry`, pin `default-llm`, validate, write `aura.json`, and
/// boot the embedded gateway.
#[tauri::command]
pub async fn finish_setup(
    app: AppHandle,
    state: State<'_, AppState>,
    req: FinishSetup,
) -> Result<Done, String> {
    if req.model.trim().is_empty() {
        return Err("model must be non-empty".into());
    }
    let (config_path, mut config) = {
        let guard = state.draft.lock();
        let draft = guard.as_ref().ok_or("setup not initialized")?;
        (draft.config_path.clone(), draft.config.clone())
    };
    let entry = LlmEntry {
        name: req.entry_name.clone().into(),
        provider: req.provider.clone(),
        model: req.model.clone(),
        api_key_env: None,
        base_url: req.base_url.clone().filter(|s| !s.trim().is_empty()),
        supports_vision: None,
        context_window: None,
        pricing: None,
        reasoning_effort: req.reasoning_effort.clone().filter(|s| !s.trim().is_empty()),
    };
    config.llm.retain(|e| e.name.as_str() != req.entry_name);
    config.llm.push(entry);
    config.default_llm = req.entry_name.clone().into();
    config
        .validate()
        .map_err(|e| format!("config invalid: {e}"))?;
    config
        .write_to_file(&config_path)
        .await
        .map_err(|e| format!("write aura.json: {e}"))?;
    if let Some(draft) = state.draft.lock().as_mut() {
        draft.config = config;
    }
    let root = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app_data_dir: {e}"))?;
    crate::trigger_boot(app, root);
    Ok(Done { ok: true })
}

/// OAuth PKCE sign-in for `openai-subscription` (docs/mac-app.md §5).
/// `pkce_login` itself binds the `127.0.0.1:1455` callback listener; we just
/// open the authorize URL in the system browser (`open`) and persist the token
/// bundle. Records the pending entry so `finish_setup` can pin it.
#[tauri::command]
pub async fn start_oauth(state: State<'_, AppState>) -> Result<OauthOutcome, String> {
    use aura_llm::providers::openai_subscription::{PROVIDER_NAME, VaultTokenStore, pkce_login};

    let vault = {
        let guard = state.draft.lock();
        guard.as_ref().ok_or("setup not initialized")?.vault.clone()
    };
    let http = aura_security::http::client(None).map_err(|e| format!("http client: {e}"))?;
    let present = |url: &str| -> std::io::Result<()> {
        std::process::Command::new("open").arg(url).spawn().map(|_| ())
    };
    let bundle = pkce_login(present, &http)
        .await
        .map_err(|e| format!("oauth: {e}"))?;
    VaultTokenStore::new(vault)
        .save(&bundle)
        .await
        .map_err(|e| format!("vault save: {e}"))?;

    let entry_name = {
        let mut guard = state.draft.lock();
        let draft = guard.as_mut().ok_or("setup not initialized")?;
        let name = unique_entry_name(PROVIDER_NAME, &draft.config.llm);
        draft.pending = Some(PendingEntry {
            entry_name: name.clone(),
            base_url: None,
        });
        name
    };
    Ok(OauthOutcome {
        entry_name,
        email: bundle.email(),
        plan: bundle.plan_type(),
    })
}

/// Boot the gateway for an already-configured install (the configured branch of
/// the lifecycle, docs/mac-app.md §5). Idempotent via the boot guard.
#[tauri::command]
pub async fn start_runtime(app: AppHandle) -> Result<(), String> {
    let root = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app_data_dir: {e}"))?;
    crate::trigger_boot(app, root);
    Ok(())
}

fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A provider-derived entry name unique within the existing config (mirrors
/// `aura_setup::flow::llm`'s `unique_default_name`).
fn unique_entry_name(provider: &str, existing: &[LlmEntry]) -> String {
    let mut candidate = provider.to_string();
    let mut n = 2;
    while existing.iter().any(|e| e.name.as_str() == candidate) {
        candidate = format!("{provider}{n}");
        n += 1;
    }
    candidate
}
