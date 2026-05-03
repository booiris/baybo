use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use aura_config::{AuraConfig, LlmEntry};
use aura_llm::credentials::{resolve_api_key, vault_api_key_name};
use aura_llm::providers::openai_subscription::{
    DeviceCode, PROVIDER_NAME as SUB_PROVIDER_NAME, VAULT_KEY_TOKENS, VaultTokenStore,
    device_code_login, pkce_login, revoke,
};
use aura_llm::{
    LiveModelInfo, LlmProviderConfig, LlmProviderRegistry, default_base_url_for_provider,
};
use aura_security::SecretVault;
use aura_workspace::paths::{ENV_CONFIG_PATH, default_config_file};
use serde_json::{Value, json};

use crate::cli::LlmCmd;
use crate::commands::prompt::{confirm, prompt_with_default};
use crate::commands::secret_input::read_masked_password;
use crate::commands::select::select_one;
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

/// Built-in providers offered by `aura llm add`.
const BUILTIN_PROVIDERS: &[&str] = &[
    "openai",
    "anthropic",
    "gemini",
    "minimax",
    SUB_PROVIDER_NAME,
];

/// Hint appended to every config-mutating output. The runtime
/// (`aura gateway start`) loads aura.json once at boot and holds the
/// snapshot for the process lifetime — there is no file watcher, so
/// any LLM change only takes effect on next restart.
const RESTART_HINT: &str =
    "\n(note: a running `aura gateway` keeps the old config; restart it to pick up)";

pub async fn handle(ctx: &CommandContext, cmd: LlmCmd) -> Result<CommandOutput> {
    match cmd {
        LlmCmd::Status => status(ctx),
        LlmCmd::Probe { name } => probe(ctx, name).await,
        LlmCmd::LiveModel { name } => live_model(ctx, name).await,
        LlmCmd::Add => add(ctx).await,
        LlmCmd::Edit => edit(ctx).await,
        LlmCmd::Remove => remove(ctx).await,
        LlmCmd::Default => set_default(ctx).await,
    }
}

fn status(ctx: &CommandContext) -> Result<CommandOutput> {
    let entries = &ctx.config.llm;
    if entries.is_empty() {
        return Ok(CommandOutput {
            human: "(no LLM entries registered)".into(),
            data: Some(json!({ "entries": [], "default": ctx.config.default_llm })),
        });
    }
    let mut human = String::new();
    let mut data: Vec<Value> = Vec::new();
    for entry in entries {
        let is_default = entry.name == ctx.config.default_llm;
        let marker = if is_default { "*" } else { " " };
        let api_src = entry
            .api_key_env
            .as_deref()
            .unwrap_or("(vault / provider default)");
        let effort_label = entry
            .reasoning_effort
            .as_deref()
            .map(|e| format!(" reasoning={e}"))
            .unwrap_or_default();
        human.push_str(&format!(
            "{marker} {name} :: provider={provider} model={model} api_key_env={api_src}{effort_label}\n",
            name = entry.name,
            provider = entry.provider,
            model = entry.model,
        ));
        data.push(json!({
            "name": entry.name,
            "provider": entry.provider,
            "model": entry.model,
            "api_key_env": entry.api_key_env,
            "base_url": entry.base_url,
            "supports_vision": entry.supports_vision,
            "reasoning_effort": entry.reasoning_effort,
            "is_default": is_default,
        }));
    }
    Ok(CommandOutput {
        human: human.trim_end().to_string(),
        data: Some(json!({
            "entries": data,
            "default": ctx.config.default_llm,
        })),
    })
}

async fn probe(ctx: &CommandContext, name: Option<String>) -> Result<CommandOutput> {
    let entries = &ctx.config.llm;
    if entries.is_empty() {
        return Err(CliError::Config(
            "no LLM entries registered; run `aura llm add` first".into(),
        ));
    }
    let entry = match name {
        Some(n) => entries
            .iter()
            .find(|e| e.name == n)
            .ok_or_else(|| CliError::Config(format!("no llm entry named {n:?}")))?,
        None => pick_entry(entries, "Select an LLM entry to probe:")?,
    };

    let registry = LlmProviderRegistry::with_default_providers();
    let cfg = LlmProviderConfig {
        provider: entry.provider.clone(),
        api_key: resolve_api_key(
            &entry.name,
            &entry.provider,
            entry.api_key_env.as_deref(),
            ctx.secret_vault.as_deref(),
        )
        .await,
        base_url: entry.base_url.clone(),
        model: entry.model.clone(),
        supports_vision: entry.supports_vision,
        reasoning_effort: entry.reasoning_effort.clone(),
        vault: ctx.secret_vault.clone(),
    };
    match registry.create_client(&cfg) {
        Err(e) => Err(CliError::Manager(format!(
            "build provider client for {}: {e}",
            entry.name
        ))),
        Ok(client) => {
            let started = Instant::now();
            match client.probe().await {
                Ok(report) => {
                    let mut human = format!(
                        "ok  entry={}  provider={}  model={}  latency={}ms  tokens={}/{} (in/out)",
                        entry.name,
                        entry.provider,
                        entry.model,
                        report.latency_ms,
                        report.tokens.input_tokens,
                        report.tokens.output_tokens,
                    );
                    if let Some(chars) = report.thinking_chars {
                        human.push_str(&format!("\nthinking  ={chars} chars"));
                        if let Some(preview) = &report.thinking_preview
                            && !preview.is_empty()
                        {
                            human.push_str(&format!(" :: {preview}"));
                        }
                    }
                    Ok(CommandOutput {
                        human,
                        data: Some(json!({
                            "name": entry.name,
                            "provider": entry.provider,
                            "model": entry.model,
                            "ok": true,
                            "latency_ms": report.latency_ms,
                            "input_tokens": report.tokens.input_tokens,
                            "output_tokens": report.tokens.output_tokens,
                            "thinking_chars": report.thinking_chars,
                            "thinking_preview": report.thinking_preview,
                        })),
                    })
                }
                Err(e) => Err(CliError::Manager(format!(
                    "probe failed for entry {} after {}ms: {e}",
                    entry.name,
                    started.elapsed().as_millis(),
                ))),
            }
        }
    }
}

async fn live_model(ctx: &CommandContext, name: Option<String>) -> Result<CommandOutput> {
    let entries = &ctx.config.llm;
    if entries.is_empty() {
        return Err(CliError::Config(
            "no LLM entries registered; run `aura llm add` first".into(),
        ));
    }
    let entry = match name {
        Some(n) => entries
            .iter()
            .find(|e| e.name == n)
            .ok_or_else(|| CliError::Config(format!("no llm entry named {n:?}")))?,
        None => pick_entry(entries, "Select an LLM entry to query:")?,
    };
    let models = fetch_live_models(entry, ctx.secret_vault.clone()).await?;
    let mut human = format!("{} (live models for {}):\n", entry.name, entry.provider);
    if models.is_empty() {
        human.push_str("  (catalog endpoint returned no models)\n");
    } else {
        for m in &models {
            human.push_str(&render_live_model(m));
        }
    }
    let data: Vec<Value> = models
        .iter()
        .map(|m| {
            json!({
                "id": m.id,
                "display_name": m.display_name,
                "description": m.description,
                "context_window": m.context_window,
                "supports_vision": m.supports_vision,
                "supports_tools": m.supports_tools,
                "extras": m.extras,
            })
        })
        .collect();
    Ok(CommandOutput {
        human: human.trim_end().to_string(),
        data: Some(json!({
            "name": entry.name,
            "provider": entry.provider,
            "models": data,
        })),
    })
}

async fn add(ctx: &CommandContext) -> Result<CommandOutput> {
    let target = resolve_target_path(ctx)?;
    let vault = ctx.secret_vault.as_ref().ok_or_else(|| {
        CliError::Config(
            "secret vault unavailable — run from the workspace root after the gateway has \
             initialised the vault at least once"
                .into(),
        )
    })?;

    require_tty()?;
    let provider_idx = select_one("Provider:", BUILTIN_PROVIDERS)?;
    let provider = BUILTIN_PROVIDERS[provider_idx].to_string();

    let default_name = unique_default_name(&provider, &ctx.config.llm);
    let name = prompt_with_default("Entry name", &default_name)?;
    if name.trim().is_empty() {
        return Err(CliError::Config("entry name must be non-empty".into()));
    }
    // Pre-check duplicates BEFORE we touch the vault. validate() at
    // the end would catch the duplicate, but by then we'd have
    // already overwritten the existing entry's vault api key (same
    // vault key per name). Better to reject here and leave the
    // vault untouched.
    if ctx.config.llm.iter().any(|e| e.name == name) {
        return Err(CliError::Config(format!(
            "entry name {name:?} already exists; use `aura llm edit` or pick a different name"
        )));
    }

    let default_base = default_base_url(&provider).unwrap_or("");
    let base_url_input = prompt_with_default("Base URL", default_base)?;
    let base_url = if base_url_input.trim().is_empty() {
        None
    } else {
        Some(base_url_input.trim().to_string())
    };

    // `api_key_env` stays unset on the new entry. Setting it to a
    // provider-default env var name would let a global
    // `OPENAI_API_KEY` mask this entry's per-entry vault secret —
    // the regression flagged by the adversarial review.
    let api_key_env: Option<String> = if provider == SUB_PROVIDER_NAME {
        run_subscription_login(vault).await?;
        None
    } else {
        let api_key = read_masked_password("API key (will be encrypted into the vault): ")?;
        if api_key.is_empty() {
            return Err(CliError::Config("api key must be non-empty".into()));
        }
        vault
            .store_secret(&vault_api_key_name(&name), api_key.as_bytes())
            .await
            .map_err(|e| CliError::Manager(format!("write api key to vault: {e}")))?;
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

    eprintln!("Fetching live model catalog…");
    let live = match fetch_live_models(&temp_entry, Some(vault.clone())).await {
        Ok(models) => models,
        Err(e) => {
            // Discovery failure shouldn't strand the operator —
            // fall back to typing the model id by hand.
            eprintln!("(live model discovery failed: {e}; falling back to manual entry)");
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

    let model_idx = select_one("Model:", &label_refs)?;
    let model = if model_idx == live.len() {
        let typed = prompt_with_default("Custom model id", "")?;
        if typed.trim().is_empty() {
            return Err(CliError::Config("model id must be non-empty".into()));
        }
        typed
    } else {
        live[model_idx].id.clone()
    };

    // Reasoning effort is meaningful only for openai-subscription
    // (Codex Responses). Other providers either lack the knob or
    // fix it server-side, so we don't even ask.
    let reasoning_effort = if provider == SUB_PROVIDER_NAME {
        let levels = aura_llm::providers::openai_subscription::allowed_efforts_for(&model);
        let labels: Vec<&str> = levels.to_vec();
        let idx = select_one("Reasoning effort:", &labels)?;
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

    let mut new_config: AuraConfig = ctx.config.as_ref().clone();
    new_config.llm.push(entry);
    // Only ask when there's something to choose between. With a
    // single entry there is no meaningful alternative, so auto-set
    // it as default. Same applies when the prior `default-llm` is
    // empty / dangling — without this, validate() would fail.
    let prior_default_valid = new_config
        .llm
        .iter()
        .any(|e| e.name == new_config.default_llm);
    let auto_promote = new_config.llm.len() == 1 || !prior_default_valid;
    let became_default = auto_promote
        || confirm(&format!(
            "Set {name:?} as default-llm? (current: {})",
            new_config.default_llm
        ))?;
    if became_default {
        new_config.default_llm = name.clone();
    }
    new_config
        .validate()
        .map_err(|e| CliError::Config(format!("config validation failed: {e}")))?;
    new_config.write_to_file(&target).await?;

    let mut human = format!(
        "added entry {name:?} (provider={provider}, model={model})\nwrote {}",
        target.display(),
    );
    if became_default {
        human.push_str(&format!("\ndefault-llm = {name}"));
    }
    human.push_str(RESTART_HINT);
    Ok(CommandOutput {
        human,
        data: Some(json!({
            "name": name,
            "provider": provider,
            "model": model,
            "is_default": became_default,
            "written_to": target.display().to_string(),
            "requires_restart": true,
        })),
    })
}

async fn edit(ctx: &CommandContext) -> Result<CommandOutput> {
    let entries = &ctx.config.llm;
    if entries.is_empty() {
        return Err(CliError::Config(
            "no LLM entries to edit; run `aura llm add` first".into(),
        ));
    }
    let target = resolve_target_path(ctx)?;
    let vault = ctx.secret_vault.as_ref().ok_or_else(|| {
        CliError::Config(
            "secret vault unavailable — run from the workspace root after the gateway has \
             initialised the vault at least once"
                .into(),
        )
    })?;
    require_tty()?;

    let pick = pick_entry(entries, "Edit which entry?")?;
    // Working copy that picks up edits across the loop.
    let mut working = pick.clone();

    // Field-picker labels — order matches the typical edit sequence.
    // Each branch updates `working` in place; "Done" exits the loop.
    let mut changed: Vec<&'static str> = Vec::new();
    loop {
        let mut options: Vec<String> = vec![
            format!("model        = {}", working.model),
            format!(
                "base_url     = {}",
                working.base_url.as_deref().unwrap_or("(default)")
            ),
        ];
        let api_key_label = if working.provider == SUB_PROVIDER_NAME {
            "OAuth login (re-authenticate)".to_string()
        } else {
            format!(
                "api_key      ({})",
                working.api_key_env.as_deref().unwrap_or("vault")
            )
        };
        options.push(api_key_label);
        if working.provider == SUB_PROVIDER_NAME {
            options.push(format!(
                "reasoning    = {}",
                working.reasoning_effort.as_deref().unwrap_or("(default)")
            ));
        }
        options.push(format!(
            "vision       = {}",
            working
                .supports_vision
                .map(|b| if b { "true" } else { "false" })
                .unwrap_or("(provider default)")
        ));
        options.push("Done — save and exit".into());
        options.push("Cancel — discard changes".into());

        let label_refs: Vec<&str> = options.iter().map(String::as_str).collect();
        let idx = select_one("Field to edit:", &label_refs)?;
        // Reconstruct the action set: the offsets shift when
        // `reasoning` is hidden, so dispatch off the picked label
        // string rather than a hard-coded index.
        let picked = label_refs[idx];

        if picked.starts_with("Done") {
            break;
        }
        if picked.starts_with("Cancel") {
            return Ok(CommandOutput {
                human: format!("cancelled: {} not modified", working.name),
                data: Some(json!({ "name": working.name, "action": "cancelled" })),
            });
        }
        if picked.starts_with("model ") {
            // Re-fetch live catalog so the operator can pick the new
            // model from a fresh list. Custom-id row stays available.
            let live = match fetch_live_models(&working, Some(vault.clone())).await {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("(live model discovery failed: {e}; falling back to manual entry)");
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
            labels.push("<enter custom model id>".into());
            let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let mi = select_one("Model:", &label_refs)?;
            let new_model = if mi == live.len() {
                let typed = prompt_with_default("Custom model id", &working.model)?;
                if typed.trim().is_empty() {
                    return Err(CliError::Config("model id must be non-empty".into()));
                }
                typed
            } else {
                live[mi].id.clone()
            };
            if new_model != working.model {
                // Reasoning effort is model-family scoped; clamp on
                // change so the persisted value is always valid.
                if working.provider == SUB_PROVIDER_NAME
                    && let Some(current) = &working.reasoning_effort
                {
                    let allowed =
                        aura_llm::providers::openai_subscription::allowed_efforts_for(&new_model);
                    if !allowed.iter().any(|e| e.eq_ignore_ascii_case(current)) {
                        eprintln!(
                            "(reasoning effort {current:?} is not valid for {new_model}; reset to default)"
                        );
                        working.reasoning_effort = None;
                    }
                }
                working.model = new_model;
                changed.push("model");
            }
        } else if picked.starts_with("base_url ") {
            let current = working.base_url.as_deref().unwrap_or("");
            let typed = prompt_with_default("Base URL (empty = use provider default)", current)?;
            let new_base = if typed.trim().is_empty() {
                None
            } else {
                Some(typed.trim().to_string())
            };
            if new_base != working.base_url {
                working.base_url = new_base;
                changed.push("base_url");
            }
        } else if picked.starts_with("api_key ") {
            let new_key = read_masked_password("New API key (will overwrite vault entry): ")?;
            if new_key.is_empty() {
                eprintln!("(empty input — api key not changed)");
            } else {
                vault
                    .store_secret(&vault_api_key_name(&working.name), new_key.as_bytes())
                    .await
                    .map_err(|e| CliError::Manager(format!("write api key to vault: {e}")))?;
                changed.push("api_key");
            }
        } else if picked.starts_with("OAuth login") {
            run_subscription_login(vault).await?;
            changed.push("oauth");
        } else if picked.starts_with("reasoning ") {
            let levels =
                aura_llm::providers::openai_subscription::allowed_efforts_for(&working.model);
            let labels: Vec<&str> = levels.to_vec();
            let li = select_one("Reasoning effort:", &labels)?;
            let new_effort = Some(levels[li].to_string());
            if new_effort != working.reasoning_effort {
                working.reasoning_effort = new_effort;
                changed.push("reasoning_effort");
            }
        } else if picked.starts_with("vision ") {
            let choices = ["(provider default)", "true", "false"];
            let vi = select_one("Vision support:", &choices)?;
            let new_vision = match vi {
                0 => None,
                1 => Some(true),
                _ => Some(false),
            };
            if new_vision != working.supports_vision {
                working.supports_vision = new_vision;
                changed.push("supports_vision");
            }
        }
    }

    if changed.is_empty() {
        return Ok(CommandOutput {
            human: format!("no changes to {}", working.name),
            data: Some(json!({ "name": working.name, "changed": [] })),
        });
    }

    let mut new_config: AuraConfig = ctx.config.as_ref().clone();
    if let Some(slot) = new_config.llm.iter_mut().find(|e| e.name == working.name) {
        *slot = working.clone();
    }
    new_config
        .validate()
        .map_err(|e| CliError::Config(format!("config validation failed: {e}")))?;
    new_config.write_to_file(&target).await?;

    let human = format!(
        "edited {} (changed: {}); wrote {}{}",
        working.name,
        changed.join(", "),
        target.display(),
        RESTART_HINT,
    );
    Ok(CommandOutput {
        human,
        data: Some(json!({
            "name": working.name,
            "changed": changed,
            "written_to": target.display().to_string(),
            "requires_restart": true,
        })),
    })
}

async fn remove(ctx: &CommandContext) -> Result<CommandOutput> {
    let entries = &ctx.config.llm;
    if entries.is_empty() {
        return Err(CliError::Config("no LLM entries to remove".into()));
    }
    let target = resolve_target_path(ctx)?;
    let vault = ctx.secret_vault.as_ref().ok_or_else(|| {
        CliError::Config(
            "secret vault unavailable — run from the workspace root after the gateway has \
             initialised the vault at least once"
                .into(),
        )
    })?;
    require_tty()?;

    let entry = pick_entry(entries, "Remove which entry?")?;
    let entry_name = entry.name.clone();
    let provider = entry.provider.clone();

    if entry_name == ctx.config.default_llm {
        return Err(CliError::Config(format!(
            "{entry_name:?} is the current default-llm; switch with `aura llm default` before removing it"
        )));
    }
    if !confirm(&format!("Delete entry {entry_name:?}?"))? {
        return Ok(CommandOutput {
            human: format!("cancelled: {entry_name:?} not removed"),
            data: Some(json!({ "name": entry_name, "action": "cancelled" })),
        });
    }

    // Best-effort vault clear. A missing key isn't an error — the
    // entry may have been added without an api key (env-only flow).
    if let Err(e) = vault.delete_secret(&vault_api_key_name(&entry_name)).await {
        tracing::warn!(
            error = %e,
            entry = %entry_name,
            "remove: failed to delete per-entry vault secret; continuing with config write"
        );
    }

    // For openai-subscription, the OAuth bundle is stored at a
    // shared key (single profile). Best-effort revoke + clear when
    // we're removing the only subscription entry — otherwise leave
    // it alone so the surviving entry keeps its login.
    let mut sub_revoked = false;
    if provider == SUB_PROVIDER_NAME {
        let other_sub_entries = ctx
            .config
            .llm
            .iter()
            .any(|e| e.name != entry_name && e.provider == SUB_PROVIDER_NAME);
        if !other_sub_entries {
            let store = VaultTokenStore::new(vault.clone());
            if let Ok(Some(bundle)) = store.load().await {
                match revoke(&bundle.refresh_token).await {
                    Ok(()) => sub_revoked = true,
                    Err(e) => tracing::warn!(
                        error = %e,
                        "remove: openai-subscription server-side revoke failed; clearing local vault entry"
                    ),
                }
            }
            if let Err(e) = store.clear().await {
                tracing::warn!(
                    error = %e,
                    "remove: openai-subscription vault clear failed"
                );
            }
        }
    }

    let mut new_config: AuraConfig = ctx.config.as_ref().clone();
    new_config.llm.retain(|e| e.name != entry_name);
    new_config
        .validate()
        .map_err(|e| CliError::Config(format!("config validation failed: {e}")))?;
    new_config.write_to_file(&target).await?;

    let human = format!(
        "removed entry {entry_name:?} (provider={provider}); wrote {}{}",
        target.display(),
        RESTART_HINT,
    );
    Ok(CommandOutput {
        human,
        data: Some(json!({
            "name": entry_name,
            "provider": provider,
            "subscription_revoked": sub_revoked,
            "written_to": target.display().to_string(),
            "requires_restart": true,
        })),
    })
}

async fn set_default(ctx: &CommandContext) -> Result<CommandOutput> {
    let entries = &ctx.config.llm;
    if entries.is_empty() {
        return Err(CliError::Config(
            "no LLM entries registered; run `aura llm add` first".into(),
        ));
    }
    let target = resolve_target_path(ctx)?;
    let entry = pick_entry(entries, "Set default-llm to:")?;
    let mut new_config: AuraConfig = ctx.config.as_ref().clone();
    new_config.default_llm = entry.name.clone();
    new_config
        .validate()
        .map_err(|e| CliError::Config(format!("config validation failed: {e}")))?;
    new_config.write_to_file(&target).await?;
    Ok(CommandOutput {
        human: format!(
            "default-llm = {} (wrote {}){}",
            entry.name,
            target.display(),
            RESTART_HINT,
        ),
        data: Some(json!({
            "default": entry.name,
            "written_to": target.display().to_string(),
            "requires_restart": true,
        })),
    })
}

/// Pretty-print every field a `LiveModelInfo` carries — id and
/// flag tags on the headline, then description and any
/// provider-specific `extras` indented underneath. Missing fields
/// are omitted (no `Option::None` lines), so providers that only
/// surface ids degrade to a single-line view.
fn render_live_model(m: &LiveModelInfo) -> String {
    let mut out = String::new();
    let mut tags: Vec<String> = Vec::new();
    if let Some(ctx) = m.context_window {
        tags.push(format!("ctx={ctx}"));
    }
    if let Some(true) = m.supports_tools {
        tags.push("tools".into());
    } else if let Some(false) = m.supports_tools {
        tags.push("no-tools".into());
    }
    if let Some(true) = m.supports_vision {
        tags.push("vision".into());
    } else if let Some(false) = m.supports_vision {
        tags.push("no-vision".into());
    }
    let tag_blob = if tags.is_empty() {
        String::new()
    } else {
        format!("  [{}]", tags.join(", "))
    };
    let display = m
        .display_name
        .as_ref()
        .filter(|n| n.as_str() != m.id)
        .map(|n| format!("  ({n})"))
        .unwrap_or_default();
    out.push_str(&format!("  {}{}{}\n", m.id, tag_blob, display));

    if let Some(desc) = m.description.as_deref()
        && !desc.is_empty()
    {
        // Single line, soft-cap at ~200 chars so the picker stays
        // legible even when providers paste their marketing copy.
        let trimmed: String = desc.chars().take(200).collect();
        let suffix = if desc.chars().count() > 200 {
            "…"
        } else {
            ""
        };
        out.push_str(&format!("      description: {trimmed}{suffix}\n"));
    }
    if !m.extras.is_null()
        && let Some(obj) = m.extras.as_object()
    {
        // Stable key ordering for diff-friendly output.
        let mut keys: Vec<&String> = obj.keys().collect();
        keys.sort();
        for key in keys {
            let value = &obj[key];
            // `serde_json::to_string` keeps numbers/bools tight; for
            // arrays/objects fall back to compact JSON so multi-key
            // values don't explode the page.
            let rendered = match value {
                Value::String(s) => s.clone(),
                Value::Null => "null".into(),
                other => other.to_string(),
            };
            // Cap each line at ~200 chars too.
            let rendered: String = rendered.chars().take(200).collect();
            out.push_str(&format!("      {key}: {rendered}\n"));
        }
    }
    out
}

fn pick_entry<'a>(entries: &'a [LlmEntry], prompt: &str) -> Result<&'a LlmEntry> {
    let labels: Vec<String> = entries
        .iter()
        .map(|e| format!("{} ({} :: {})", e.name, e.provider, e.model))
        .collect();
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let idx = select_one(prompt, &refs)?;
    Ok(&entries[idx])
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

fn default_base_url(provider: &str) -> Option<&'static str> {
    default_base_url_for_provider(provider)
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
        .map_err(|e| CliError::Manager(format!("live model discovery: {e}")))
}

fn resolve_target_path(ctx: &CommandContext) -> Result<PathBuf> {
    ctx.config_path
        .clone()
        .or_else(|| std::env::var(ENV_CONFIG_PATH).ok().map(PathBuf::from))
        .or_else(|| Some(default_config_file()))
        .ok_or_else(|| {
            CliError::Config(format!(
                "no config file resolved; set {ENV_CONFIG_PATH} (or pass --config <path>)"
            ))
        })
}

fn require_tty() -> Result<()> {
    let stdin = std::io::stdin();
    let stderr = std::io::stderr();
    if !stdin.is_terminal() || !stderr.is_terminal() {
        return Err(CliError::Config(
            "interactive llm command requires a terminal".into(),
        ));
    }
    Ok(())
}

async fn run_subscription_login(vault: &Arc<SecretVault>) -> Result<()> {
    let store = VaultTokenStore::new(vault.clone());
    // PKCE wants a browser that can reach 127.0.0.1; device code
    // only needs a browser somewhere. On a headless SSH box the TTY
    // is fine but the localhost callback won't reach a browser, so
    // we ask rather than auto-pick on is_terminal alone.
    let methods = ["PKCE (open browser, localhost callback)", "Device code"];
    let bundle = if !std::io::stdin().is_terminal() {
        device_code_login(print_device_code).await
    } else {
        match select_one("Login method:", &methods)? {
            0 => pkce_login(print_pkce_url).await,
            _ => device_code_login(print_device_code).await,
        }
    }
    .map_err(|e| CliError::Manager(format!("openai-subscription login: {e}")))?;
    store
        .save(&bundle)
        .await
        .map_err(|e| CliError::Manager(format!("openai-subscription vault save: {e}")))?;
    eprintln!(
        "signed in as {} (plan: {})\ntoken stored at vault://{}",
        bundle.email().unwrap_or_else(|| "<unknown>".into()),
        bundle.plan_type().unwrap_or_else(|| "<unknown>".into()),
        VAULT_KEY_TOKENS,
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
