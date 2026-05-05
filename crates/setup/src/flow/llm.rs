//! LLM provider step. Mutates `AuraConfig::llm` in place; never writes
//! `aura.json` itself.

use std::sync::Arc;

use aura_config::{AuraConfig, LlmEntry};
use aura_llm::credentials::{resolve_api_key, vault_api_key_name};
use aura_llm::providers::openai_subscription::{
    DeviceCode, PROVIDER_NAME as SUB_PROVIDER_NAME, VAULT_KEY_TOKENS, VaultTokenStore,
    device_code_login, pkce_login,
};
use aura_llm::{
    LiveModelInfo, LlmProviderConfig, LlmProviderRegistry, default_base_url_for_provider,
};
use aura_security::SecretVault;

use crate::error::{Result, SetupError};
use crate::prompt::Prompter;

const BUILTIN_PROVIDERS: &[&str] = &[
    "openai",
    "anthropic",
    "gemini",
    "minimax",
    SUB_PROVIDER_NAME,
];

#[derive(Debug, Clone, PartialEq)]
pub enum LlmStepOutcome {
    Added(LlmEntry),
    Skipped,
}

pub async fn configure_llm_step<P: Prompter>(
    prompter: &mut P,
    vault: &Arc<SecretVault>,
    config: &mut AuraConfig,
    allow_skip: bool,
) -> Result<LlmStepOutcome> {
    let has_existing = !config.llm.is_empty();
    let label = if has_existing {
        "LLM step: an entry is already configured."
    } else {
        "LLM step:"
    };
    let add_label = if has_existing {
        "Add another LLM entry"
    } else {
        "Configure an LLM provider now"
    };
    if !crate::flow::pick_add_or_skip(
        prompter,
        label,
        add_label,
        "Skip — configure later with `aura llm add`",
        allow_skip,
    )? {
        return Ok(LlmStepOutcome::Skipped);
    }
    add_entry(prompter, vault, config).await
}

async fn add_entry<P: Prompter>(
    prompter: &mut P,
    vault: &Arc<SecretVault>,
    config: &mut AuraConfig,
) -> Result<LlmStepOutcome> {
    let provider_idx = prompter.select("Provider:", BUILTIN_PROVIDERS)?;
    let provider = BUILTIN_PROVIDERS[provider_idx].to_string();

    let default_name = unique_default_name(&provider, &config.llm);
    let name = prompter.text("Entry name", &default_name)?;
    if name.trim().is_empty() {
        return Err(SetupError::Llm("entry name must be non-empty".into()));
    }
    if config.llm.iter().any(|e| e.name == name) {
        return Err(SetupError::Llm(format!(
            "entry name {name:?} already exists; pick a different name"
        )));
    }

    let default_base = default_base_url_for_provider(&provider).unwrap_or("");
    let base_url_input = prompter.text("Base URL", default_base)?;
    let base_url = if base_url_input.trim().is_empty() {
        None
    } else {
        Some(base_url_input.trim().to_string())
    };

    // `api_key_env` stays unset on the new entry — setting it would
    // let a global `OPENAI_API_KEY` mask the per-entry vault secret.
    let api_key_env: Option<String> = if provider == SUB_PROVIDER_NAME {
        run_subscription_login(prompter, vault).await?;
        None
    } else {
        let api_key = prompter.password("API key (will be encrypted into the vault): ")?;
        if api_key.is_empty() {
            return Err(SetupError::Llm("api key must be non-empty".into()));
        }
        vault
            .store_secret(&vault_api_key_name(&name), api_key.as_bytes())
            .await
            .map_err(|e| SetupError::Vault(format!("write api key: {e}")))?;
        None
    };

    let temp_entry = LlmEntry {
        name: name.clone(),
        provider: provider.clone(),
        model: String::from("(unset)"),
        api_key_env: api_key_env.clone(),
        base_url: base_url.clone(),
        supports_vision: None,
        reasoning_effort: None,
    };

    tracing::info!("Fetching live model catalog…");
    let live = match fetch_live_models(&temp_entry, Some(vault.clone())).await {
        Ok(models) => models,
        Err(e) => {
            tracing::info!("(live model discovery failed: {e}; falling back to manual entry)");
            Vec::new()
        }
    };

    let mut labels: Vec<String> = live
        .iter()
        .map(|m| match (&m.display_name, m.context_window) {
            (Some(d), Some(c)) if d != &m.id => format!("{}  [{c}]  ({d})", m.id),
            (_, Some(c)) => format!("{}  [{c}]", m.id),
            (Some(d), _) if d != &m.id => format!("{}  ({d})", m.id),
            _ => m.id.clone(),
        })
        .collect();
    const CUSTOM_LABEL: &str = "<enter custom model id>";
    labels.push(CUSTOM_LABEL.to_string());
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();

    let model_idx = prompter.select("Model:", &label_refs)?;
    let model = if model_idx == live.len() {
        let typed = prompter.text("Custom model id", "")?;
        if typed.trim().is_empty() {
            return Err(SetupError::Llm("model id must be non-empty".into()));
        }
        typed
    } else {
        live[model_idx].id.clone()
    };

    // Reasoning effort: only meaningful for openai-subscription.
    let reasoning_effort = if provider == SUB_PROVIDER_NAME {
        let levels = aura_llm::providers::openai_subscription::allowed_efforts_for(&model);
        let labels: Vec<&str> = levels.to_vec();
        let idx = prompter.select("Reasoning effort:", &labels)?;
        Some(levels[idx].to_string())
    } else {
        None
    };

    let entry = LlmEntry {
        name: name.clone(),
        provider: provider.clone(),
        model: model.clone(),
        api_key_env,
        base_url,
        supports_vision: None,
        reasoning_effort,
    };

    config.llm.push(entry.clone());

    // Promote default-llm when there's only one entry (no real choice)
    // OR when the prior default is empty / dangling.
    let prior_default_valid = config.llm.iter().any(|e| e.name == config.default_llm);
    let auto_promote = config.llm.len() == 1 || !prior_default_valid;
    let became_default = if auto_promote {
        true
    } else {
        prompter.confirm(
            &format!(
                "Set {name:?} as default-llm? (current: {})",
                config.default_llm
            ),
            false,
        )?
    };
    if became_default {
        config.default_llm = name.clone();
    }

    config
        .validate()
        .map_err(|e| SetupError::Config(format!("config validation failed: {e}")))?;

    Ok(LlmStepOutcome::Added(entry))
}

fn unique_default_name(provider: &str, existing: &[LlmEntry]) -> String {
    let mut candidate = provider.to_string();
    let mut suffix = 2;
    while existing.iter().any(|e| e.name == candidate) {
        candidate = format!("{provider}{suffix}");
        suffix += 1;
    }
    candidate
}

async fn fetch_live_models(
    entry: &LlmEntry,
    vault: Option<Arc<SecretVault>>,
) -> Result<Vec<LiveModelInfo>> {
    let registry = LlmProviderRegistry::with_default_providers();
    let cfg = LlmProviderConfig {
        provider: entry.provider.clone(),
        api_key: resolve_api_key(
            &entry.name,
            &entry.provider,
            entry.api_key_env.as_deref(),
            vault.as_deref(),
        )
        .await,
        base_url: entry.base_url.clone(),
        model: if entry.model.is_empty() {
            "unused".into()
        } else {
            entry.model.clone()
        },
        supports_vision: entry.supports_vision,
        reasoning_effort: entry.reasoning_effort.clone(),
        vault,
    };
    registry
        .list_live_models(&cfg)
        .await
        .map_err(|e| SetupError::Llm(format!("live model discovery: {e}")))
}

async fn run_subscription_login<P: Prompter>(
    prompter: &mut P,
    vault: &Arc<SecretVault>,
) -> Result<()> {
    let store = VaultTokenStore::new(vault.clone());
    // PKCE wants a browser that can reach 127.0.0.1; device code only
    // needs a browser somewhere. Both modes are offered so the user
    // can pick what their environment supports.
    let methods = ["PKCE (open browser, localhost callback)", "Device code"];
    let bundle = match prompter.select("Login method:", &methods)? {
        0 => pkce_login(print_pkce_url).await,
        _ => device_code_login(print_device_code).await,
    }
    .map_err(|e| SetupError::Llm(format!("openai-subscription login: {e}")))?;
    store
        .save(&bundle)
        .await
        .map_err(|e| SetupError::Vault(format!("openai-subscription vault save: {e}")))?;
    tracing::info!(
        email = bundle.email().unwrap_or_else(|| "<unknown>".into()),
        plan = bundle.plan_type().unwrap_or_else(|| "<unknown>".into()),
        vault_key = %VAULT_KEY_TOKENS,
        "openai-subscription sign-in complete"
    );
    Ok(())
}

fn print_pkce_url(url: &str) -> std::io::Result<()> {
    println!("\nopen this URL to sign in:\n  {url}\n");
    Ok(())
}

fn print_device_code(code: &DeviceCode) {
    println!(
        "\nopen this URL on any device, sign in, and enter the code:\n  url:  {}\n  code: {}\n",
        code.verification_url, code.user_code,
    );
}

#[cfg(test)]
mod tests {
    use crate::flow::pick_add_or_skip;
    use crate::test_support::MockPrompter;

    #[test]
    fn first_run_no_skip_returns_add_without_prompt() {
        let mut prompter = MockPrompter::new();
        let go = pick_add_or_skip(&mut prompter, "L:", "add", "skip", false).unwrap();
        assert!(go);
        assert!(prompter.selects.is_empty());
    }

    #[test]
    fn re_run_skip_returns_skip() {
        let mut prompter = MockPrompter::new().push_select(1);
        let go = pick_add_or_skip(&mut prompter, "L:", "add", "skip", true).unwrap();
        assert!(!go);
    }

    #[test]
    fn re_run_add_returns_add() {
        let mut prompter = MockPrompter::new().push_select(0);
        let go = pick_add_or_skip(&mut prompter, "L:", "add", "skip", true).unwrap();
        assert!(go);
    }
}
