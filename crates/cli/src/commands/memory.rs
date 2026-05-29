//! `aura memory` subcommands — inspect / configure / test / set-key /
//! disable the pluggable memory backend.
//!
//! Memory config is **not** hot-reload (`reload.rs` classifies it as
//! non-hot, matching the trait's process-singleton invariant). Each
//! mutating command prints a `(restart aura to apply this change)` hint
//! after persisting.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use aura_config::{AuraConfig, MemoryProvider};
use aura_memory::{mem0, openviking};
use aura_security::SecretVault;
use aura_workspace::paths::{ENV_CONFIG_PATH, default_config_file};
use serde_json::{Value, json};

use crate::cli::MemoryCmd;
use crate::commands::prompt::{confirm, prompt_with_default};
use crate::commands::secret_input::read_masked_password;
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

const RESTART_HINT: &str =
    "\n(restart aura to apply this change — memory config is not hot-reload)";

pub async fn handle(ctx: &CommandContext, cmd: MemoryCmd) -> Result<CommandOutput> {
    match cmd {
        MemoryCmd::Status => status(ctx).await,
        MemoryCmd::Configure => configure(ctx).await,
        MemoryCmd::Test => test(ctx).await,
        MemoryCmd::SetKey => set_key(ctx).await,
        MemoryCmd::Disable => disable(ctx).await,
    }
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

async fn status(ctx: &CommandContext) -> Result<CommandOutput> {
    let mem = &ctx.config.memory;
    let provider_label = provider_label(mem.provider);
    let extra = if mem.extra.is_null() {
        json!({})
    } else {
        sanitize_extra(&mem.extra)
    };

    let mut human = format!(
        "enabled  = {}\nprovider = {}\n",
        mem.enabled, provider_label
    );
    let key_status = describe_key_status(ctx, mem.provider, &mem.extra).await;
    human.push_str(&format!("api_key  = {key_status}\n"));
    if let Some(llm) = &mem.llm {
        human.push_str(&format!("llm      = {llm} (unused by mem0/openviking)\n"));
    }
    if !extra.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        human.push_str(&format!(
            "extra    = {}\n",
            serde_json::to_string_pretty(&extra).unwrap_or_default()
        ));
    }

    Ok(CommandOutput {
        human: human.trim_end().to_string(),
        data: Some(json!({
            "enabled": mem.enabled,
            "provider": provider_label,
            "api_key": key_status,
            "extra": extra,
        })),
    })
}

fn provider_label(p: MemoryProvider) -> &'static str {
    match p {
        MemoryProvider::Noop => "noop",
        MemoryProvider::Mem0 => "mem0",
        MemoryProvider::OpenViking => "openviking",
    }
}

async fn describe_key_status(
    ctx: &CommandContext,
    provider: MemoryProvider,
    extra: &Value,
) -> String {
    match provider {
        MemoryProvider::Noop => "(not required)".into(),
        MemoryProvider::Mem0 => {
            let cfg = mem0::parse_extra(extra).unwrap_or_default();
            match mem0::resolve_api_key(&cfg, ctx.secret_vault.as_deref()).await {
                Some(k) if !k.is_empty() => format!("set (length {})", k.len()),
                _ => "MISSING — set MEM0_API_KEY, configure api_key_env, or `aura memory set-key`"
                    .into(),
            }
        }
        MemoryProvider::OpenViking => {
            let cfg = openviking::parse_extra(extra).unwrap_or_default();
            let k = openviking::resolve_api_key(&cfg, ctx.secret_vault.as_deref()).await;
            if k.is_empty() {
                "unset (running unauthenticated — local dev mode)".into()
            } else {
                format!("set (length {})", k.len())
            }
        }
    }
}

fn sanitize_extra(extra: &Value) -> Value {
    let mut out = extra.clone();
    if let Some(obj) = out.as_object_mut() {
        for key in ["api_key", "apiKey", "secret"] {
            if obj.contains_key(key) {
                obj.insert(key.into(), Value::String("***".into()));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// configure
// ---------------------------------------------------------------------------

async fn configure(ctx: &CommandContext) -> Result<CommandOutput> {
    require_tty()?;
    let target = resolve_target_path(ctx)?;
    let mut new_config: AuraConfig = ctx.config.as_ref().clone();

    let current = provider_label(new_config.memory.provider);
    let provider_input = prompt_with_default("Provider [mem0/openviking/noop]", current)?;
    let provider = match provider_input.trim() {
        "mem0" => MemoryProvider::Mem0,
        "openviking" => MemoryProvider::OpenViking,
        "noop" => MemoryProvider::Noop,
        other => {
            return Err(CliError::Config(format!(
                "unknown provider {other:?} (expected mem0 / openviking / noop)"
            )));
        }
    };
    new_config.memory.provider = provider;
    new_config.memory.enabled = provider != MemoryProvider::Noop;

    match provider {
        MemoryProvider::Noop => {
            new_config.memory.extra = Value::Null;
        }
        MemoryProvider::Mem0 => {
            let mut cfg = mem0::parse_extra(&new_config.memory.extra).unwrap_or_default();
            let agent_id =
                prompt_with_default("agent_id", cfg.agent_id.as_deref().unwrap_or("aura"))?;
            cfg.agent_id = if agent_id == "aura" {
                None
            } else {
                Some(agent_id)
            };

            let rerank = prompt_with_default(
                "rerank [true/false]",
                if cfg.rerank.unwrap_or(true) {
                    "true"
                } else {
                    "false"
                },
            )?;
            cfg.rerank = Some(matches!(rerank.as_str(), "true" | "yes" | "y" | "1"));

            let top_k_in = prompt_with_default("top_k", "5")?;
            cfg.top_k = top_k_in.parse::<usize>().ok();

            let base_url_in = prompt_with_default(
                "base_url (leave blank for default https://api.mem0.ai)",
                cfg.base_url.as_deref().unwrap_or(""),
            )?;
            cfg.base_url = if base_url_in.is_empty() {
                None
            } else {
                Some(base_url_in)
            };

            let api_key_env_in = prompt_with_default(
                "api_key_env (env var name; blank → use vault / MEM0_API_KEY)",
                cfg.api_key_env.as_deref().unwrap_or(""),
            )?;
            cfg.api_key_env = if api_key_env_in.is_empty() {
                None
            } else {
                Some(api_key_env_in)
            };

            new_config.memory.extra = serde_json::to_value(&cfg)
                .map_err(|e| CliError::Config(format!("serialise mem0 extra: {e}")))?;

            if cfg.api_key_env.is_none()
                && confirm("Store API key in vault now (memory.mem0.api_key)?")?
            {
                store_vault_key(ctx, "memory.mem0.api_key", "Mem0 API key").await?;
            }
        }
        MemoryProvider::OpenViking => {
            let mut cfg = openviking::parse_extra(&new_config.memory.extra).unwrap_or_default();
            let endpoint = prompt_with_default(
                "endpoint",
                cfg.endpoint.as_deref().unwrap_or("http://127.0.0.1:1933"),
            )?;
            cfg.endpoint = Some(endpoint);

            let account =
                prompt_with_default("account", cfg.account.as_deref().unwrap_or("default"))?;
            cfg.account = Some(account);

            let agent = prompt_with_default("agent", cfg.agent.as_deref().unwrap_or("aura"))?;
            cfg.agent = Some(agent);

            let top_k_in = prompt_with_default("top_k", "5")?;
            cfg.top_k = top_k_in.parse::<usize>().ok();

            let api_key_env_in = prompt_with_default(
                "api_key_env (env var name; blank → use vault / OPENVIKING_API_KEY)",
                cfg.api_key_env.as_deref().unwrap_or(""),
            )?;
            cfg.api_key_env = if api_key_env_in.is_empty() {
                None
            } else {
                Some(api_key_env_in)
            };

            new_config.memory.extra = serde_json::to_value(&cfg)
                .map_err(|e| CliError::Config(format!("serialise openviking extra: {e}")))?;

            if cfg.api_key_env.is_none()
                && confirm(
                    "Store API key in vault now (memory.openviking.api_key)? \
                          (leave blank below if running local dev mode without auth)",
                )?
            {
                store_vault_key(ctx, "memory.openviking.api_key", "OpenViking API key").await?;
            }
        }
    }

    new_config.write_to_file(&target).await?;

    let mut human = format!(
        "configured memory: enabled={} provider={}\nwrote {}",
        new_config.memory.enabled,
        provider_label(new_config.memory.provider),
        target.display(),
    );
    human.push_str(RESTART_HINT);
    Ok(CommandOutput {
        human,
        data: Some(json!({
            "enabled": new_config.memory.enabled,
            "provider": provider_label(new_config.memory.provider),
            "written_to": target.display().to_string(),
            "requires_restart": true,
        })),
    })
}

async fn store_vault_key(ctx: &CommandContext, key_name: &str, label: &str) -> Result<()> {
    let vault = ctx.secret_vault.as_ref().ok_or_else(|| {
        CliError::Config("secret vault unavailable — run from workspace root".into())
    })?;
    let value = read_masked_password(&format!("{label}: "))?;
    if value.is_empty() {
        return Ok(());
    }
    vault
        .store_secret(key_name, value.as_bytes())
        .await
        .map_err(|e| CliError::Config(format!("vault store {key_name}: {e}")))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// test
// ---------------------------------------------------------------------------

async fn test(ctx: &CommandContext) -> Result<CommandOutput> {
    let mem = &ctx.config.memory;
    if !mem.enabled || mem.provider == MemoryProvider::Noop {
        return Ok(CommandOutput {
            human: "memory disabled — nothing to test".into(),
            data: Some(json!({"status": "disabled"})),
        });
    }
    match mem.provider {
        MemoryProvider::Noop => unreachable!(),
        MemoryProvider::Mem0 => {
            let cfg = mem0::parse_extra(&mem.extra)?;
            let key = mem0::resolve_api_key(&cfg, ctx.secret_vault.as_deref())
                .await
                .unwrap_or_default();
            if key.is_empty() {
                return Err(CliError::Config(
                    "mem0 API key missing — run `aura memory set-key` first".into(),
                ));
            }
            let m = mem0::Mem0Memory::new(cfg, key)
                .map_err(|e| CliError::Config(format!("construct mem0: {e}")))?;
            m.probe().await;
            Ok(CommandOutput {
                human: "mem0 probe completed (warnings, if any, were logged)".into(),
                data: Some(json!({"provider": "mem0", "status": "probed"})),
            })
        }
        MemoryProvider::OpenViking => {
            let cfg = openviking::parse_extra(&mem.extra)?;
            let key = openviking::resolve_api_key(&cfg, ctx.secret_vault.as_deref()).await;
            let m = openviking::OpenVikingMemory::new(cfg, key)
                .map_err(|e| CliError::Config(format!("construct openviking: {e}")))?;
            m.probe().await;
            Ok(CommandOutput {
                human: "openviking /health probe completed (warnings, if any, were logged)".into(),
                data: Some(json!({"provider": "openviking", "status": "probed"})),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// set-key
// ---------------------------------------------------------------------------

async fn set_key(ctx: &CommandContext) -> Result<CommandOutput> {
    require_tty()?;
    let provider = ctx.config.memory.provider;
    let (key_name, label) = match provider {
        MemoryProvider::Mem0 => ("memory.mem0.api_key", "Mem0 API key"),
        MemoryProvider::OpenViking => ("memory.openviking.api_key", "OpenViking API key"),
        MemoryProvider::Noop => {
            return Err(CliError::Config(
                "no provider configured — run `aura memory configure` first".into(),
            ));
        }
    };
    store_vault_key(ctx, key_name, label).await?;
    Ok(CommandOutput {
        human: format!("stored {key_name} in vault.{RESTART_HINT}"),
        data: Some(json!({"vault_key": key_name, "requires_restart": true})),
    })
}

// ---------------------------------------------------------------------------
// disable
// ---------------------------------------------------------------------------

async fn disable(ctx: &CommandContext) -> Result<CommandOutput> {
    let target = resolve_target_path(ctx)?;
    let mut new_config: AuraConfig = ctx.config.as_ref().clone();
    new_config.memory.enabled = false;
    new_config.memory.provider = MemoryProvider::Noop;
    new_config.memory.extra = Value::Null;
    new_config.write_to_file(&target).await?;
    Ok(CommandOutput {
        human: format!(
            "memory disabled (provider = noop)\nwrote {}{RESTART_HINT}",
            target.display()
        ),
        data: Some(json!({"enabled": false, "provider": "noop", "requires_restart": true})),
    })
}

// ---------------------------------------------------------------------------
// helpers (mirrors `commands::llm` exactly)
// ---------------------------------------------------------------------------

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
            "interactive memory command requires a terminal".into(),
        ));
    }
    Ok(())
}

// Silence unused-imports in some configurations.
#[allow(dead_code)]
fn _arc_ref(_: &Arc<SecretVault>) {}

impl From<aura_memory::MemoryError> for CliError {
    fn from(err: aura_memory::MemoryError) -> Self {
        CliError::Config(format!("memory: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_label_round_trip() {
        assert_eq!(provider_label(MemoryProvider::Noop), "noop");
        assert_eq!(provider_label(MemoryProvider::Mem0), "mem0");
        assert_eq!(provider_label(MemoryProvider::OpenViking), "openviking");
    }

    #[test]
    fn sanitize_extra_redacts_secret_keys() {
        let extra = json!({"api_key": "abc", "agent_id": "test"});
        let sanitized = sanitize_extra(&extra);
        assert_eq!(sanitized["api_key"], "***");
        assert_eq!(sanitized["agent_id"], "test");
    }
}
