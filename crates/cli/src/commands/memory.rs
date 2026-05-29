//! `aura memory` subcommands — inspect / setup / test / set-key /
//! disable the pluggable memory backend.
//!
//! Memory config is **not** hot-reload (`reload.rs` classifies it as
//! non-hot, matching the trait's process-singleton invariant). Each
//! mutating command prints a `(restart aura to apply this change)` hint
//! after persisting.

use std::io::IsTerminal;
use std::path::PathBuf;

use aura_config::{AuraConfig, MemoryProvider};
use aura_memory::{mem0, openviking};
use aura_workspace::paths::{ENV_CONFIG_PATH, default_config_file};
use serde_json::{Value, json};

use crate::cli::MemoryCmd;
use crate::commands::prompt::prompt_with_default;
use crate::commands::select::select_one;
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

const RESTART_HINT: &str =
    "\n(restart aura to apply this change — memory config is not hot-reload)";

pub async fn handle(ctx: &CommandContext, cmd: MemoryCmd) -> Result<CommandOutput> {
    match cmd {
        MemoryCmd::Status => status(ctx).await,
        MemoryCmd::Setup => setup(ctx).await,
        MemoryCmd::Test => test(ctx).await,
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
            let name = cfg.api_key_name.as_deref().unwrap_or("MEM0_API_KEY");
            match mem0::resolve_api_key(&cfg, ctx.secret_vault.as_deref()).await {
                Some(k) if !k.is_empty() => format!("set (length {}; key '{name}')", k.len()),
                _ => format!("MISSING — run `aura secret add {name}` (or set the {name} env var)"),
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
// setup
// ---------------------------------------------------------------------------

const PROVIDER_CHOICES: &[(MemoryProvider, &str, &str)] = &[
    (
        MemoryProvider::Mem0,
        "mem0",
        "Hosted SaaS — mem0.ai (requires API key)",
    ),
    (
        MemoryProvider::OpenViking,
        "openviking",
        "Self-hosted OpenViking server (loopback by default)",
    ),
    (
        MemoryProvider::Noop,
        "noop",
        "Disabled — no recall, no write",
    ),
];

async fn setup(ctx: &CommandContext) -> Result<CommandOutput> {
    require_tty()?;
    let target = resolve_target_path(ctx)?;
    let mut new_config: AuraConfig = ctx.config.as_ref().clone();

    // Single-select radio list — no typing. Seed the highlight on the
    // currently-configured provider so re-running the wizard with Enter
    // keeps the existing choice.
    let labels: Vec<String> = PROVIDER_CHOICES
        .iter()
        .map(|(p, slug, descr)| {
            let marker = if *p == new_config.memory.provider {
                " (current)"
            } else {
                ""
            };
            format!("{slug:<11} — {descr}{marker}")
        })
        .collect();
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let picked = select_one("Provider:", &label_refs)?;
    let provider = PROVIDER_CHOICES[picked].0;

    new_config.memory.provider = provider;
    new_config.memory.enabled = provider != MemoryProvider::Noop;

    // Minimal-prompts wizard: ask only what's load-bearing for first-time
    // setup. Power users can edit `aura.json` directly for the rest of the
    // typed config; the actual API-key value is now delegated to
    // `aura secret add <name>` (no separate set-key subcommand).
    let mut secret_hint: Option<String> = None;
    match provider {
        MemoryProvider::Noop => {
            new_config.memory.extra = Value::Null;
        }
        MemoryProvider::Mem0 => {
            let cfg = mem0::parse_extra(&new_config.memory.extra).unwrap_or_default();
            let name = cfg
                .api_key_name
                .clone()
                .unwrap_or_else(|| "MEM0_API_KEY".into());
            new_config.memory.extra = serde_json::to_value(&cfg)
                .map_err(|e| CliError::Config(format!("serialise mem0 extra: {e}")))?;
            secret_hint = Some(api_key_setup_hint(ctx, &name).await);
        }
        MemoryProvider::OpenViking => {
            // The crate-internal default; matches OpenVikingConfig's None
            // fallback. If the user accepts this verbatim we leave the
            // field as `None` so `extra` stays empty.
            const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:1933";
            let mut cfg = openviking::parse_extra(&new_config.memory.extra).unwrap_or_default();
            let default_endpoint = cfg
                .endpoint
                .clone()
                .unwrap_or_else(|| DEFAULT_ENDPOINT.into());
            let endpoint = prompt_with_default("Endpoint", &default_endpoint)?;
            cfg.endpoint = if endpoint == DEFAULT_ENDPOINT {
                None
            } else {
                Some(endpoint.clone())
            };
            let name = cfg
                .api_key_name
                .clone()
                .unwrap_or_else(|| "OPENVIKING_API_KEY".into());
            new_config.memory.extra = serde_json::to_value(&cfg)
                .map_err(|e| CliError::Config(format!("serialise openviking extra: {e}")))?;

            // Local dev (loopback) runs unauthenticated — skip the hint.
            // Remote endpoints warrant the secret.
            if !is_loopback_endpoint(&endpoint) {
                secret_hint = Some(api_key_setup_hint(ctx, &name).await);
            }
        }
    }

    new_config.write_to_file(&target).await?;

    let mut human = format!(
        "memory set up: enabled={} provider={}\nwrote {}",
        new_config.memory.enabled,
        provider_label(new_config.memory.provider),
        target.display(),
    );
    if let Some(hint) = &secret_hint {
        human.push_str("\n\n");
        human.push_str(hint);
    }
    human.push_str(RESTART_HINT);
    Ok(CommandOutput {
        human,
        data: Some(json!({
            "enabled": new_config.memory.enabled,
            "provider": provider_label(new_config.memory.provider),
            "written_to": target.display().to_string(),
            "requires_restart": true,
            "api_key_hint": secret_hint,
        })),
    })
}

/// One-line hint surfaced after `aura memory setup` tells the user
/// whether the named secret is already populated or needs to be
/// added via `aura secret add <name>`. Looks up `user_env.<name>` —
/// the same vault path the backends use at startup.
async fn api_key_setup_hint(ctx: &CommandContext, name: &str) -> String {
    let already_set = match ctx.secret_vault.as_ref() {
        Some(vault) => {
            let key = format!("{}{name}", aura_security::USER_SECRET_PREFIX);
            matches!(vault.get_secret(&key).await, Ok(Some(_)))
        }
        None => false,
    };
    if already_set {
        format!("API key already present in vault as {name}.")
    } else {
        format!(
            "Run `aura secret add {name}` to provide the API key \
             (or set the {name} env var)."
        )
    }
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
            let proxy = ctx.proxy_settings();
            let m = mem0::Mem0Memory::new(cfg, key, proxy.as_ref())
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
            let proxy = ctx.proxy_settings();
            let m = openviking::OpenVikingMemory::new(cfg, key, proxy.as_ref())
                .map_err(|e| CliError::Config(format!("construct openviking: {e}")))?;
            m.probe().await;
            Ok(CommandOutput {
                human: "openviking /health probe completed (warnings, if any, were logged)".into(),
                data: Some(json!({"provider": "openviking", "status": "probed"})),
            })
        }
    }
}

/// Hostname / IP-literal heuristic for "this OpenViking server is running
/// on the local box" — used to skip the API-key prompt during `setup`,
/// since the bundled local-dev mode is unauthenticated.
fn is_loopback_endpoint(endpoint: &str) -> bool {
    let Ok(parsed) = url::Url::parse(endpoint) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let host_lc = host.to_ascii_lowercase();
    matches!(
        host_lc.as_str(),
        "localhost" | "localhost.localdomain" | "127.0.0.1" | "::1"
    ) || host_lc.ends_with(".localhost")
        || host_lc.starts_with("127.")
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

    #[test]
    fn is_loopback_endpoint_matches_common_local_hosts() {
        for ep in [
            "http://localhost:1933",
            "https://localhost",
            "http://127.0.0.1:1933",
            "http://127.5.6.7/",
            "http://[::1]:1933",
            "http://my.localhost:8080",
        ] {
            assert!(is_loopback_endpoint(ep), "should match: {ep}");
        }
        for ep in [
            "https://api.mem0.ai",
            "http://10.0.0.5:1933",
            "http://example.com",
            "not-a-url",
        ] {
            assert!(!is_loopback_endpoint(ep), "should not match: {ep}");
        }
    }
}
