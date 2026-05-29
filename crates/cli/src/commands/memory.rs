//! `aura memory` subcommands — inspect / setup / test / disable the
//! pluggable memory backend. The API-key value itself rides through
//! the shared `aura secret add <NAME>` (looking up
//! `user_env.<NAME>` in the vault).
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
use crate::commands::secret_input::read_masked_password;
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
    let previous_provider = new_config.memory.provider;

    new_config.memory.provider = provider;
    new_config.memory.enabled = provider != MemoryProvider::Noop;

    // Provider switch wipes `extra`. Mem0Config and OpenVikingConfig share
    // field names (`api_key_name`, `top_k`) — leaving the old JSON in
    // place would let the previous provider's values silently bleed into
    // the new one's parse_extra. Same provider keeps it (re-running setup
    // preserves prior choices).
    if previous_provider != provider {
        new_config.memory.extra = Value::Null;
    }

    // Detailed wizard: walk every typed field with the existing value as
    // the bracketed default, so the operator can blow through with Enter
    // to accept or type to override.
    //
    // For every field, an empty input reverts to `None`, which lets the
    // typed default kick in and keeps `extra` JSON tidy (see the
    // `skip_serializing_if` contract on each config struct). The API-key
    // *value* is prompted at the end of the per-provider block and
    // stored at `user_env.<name>` — the same path `aura secret add`
    // writes to, so the operator can later rotate/inspect it through
    // the existing secret CLI without learning a memory-specific surface.
    let mut secret_outcome: Option<SecretWriteOutcome> = None;
    match provider {
        MemoryProvider::Noop => {
            new_config.memory.extra = Value::Null;
        }
        MemoryProvider::Mem0 => {
            let mut cfg = mem0::parse_extra(&new_config.memory.extra).unwrap_or_default();

            cfg.base_url = prompt_optional(
                "base_url (blank for https://api.mem0.ai)",
                "",
                cfg.base_url.as_deref(),
            )?;

            cfg.rerank = prompt_bool(
                "rerank — Mem0-side reranking (more accurate, slower)",
                cfg.rerank.unwrap_or(true),
            )?;

            cfg.top_k = prompt_usize("top_k — max memories per recall", cfg.top_k.unwrap_or(5))?;

            // The user-secret name is intentionally not asked: the default
            // `MEM0_API_KEY` matches the Mem0 docs' env-var convention and
            // covers every realistic deployment. We DO persist the resolved
            // name into `extra.api_key_name` even when it equals the
            // built-in default — so opening `aura.json` shows the operator
            // exactly which vault entry (`user_env.<NAME>`) and which env
            // var the runtime will resolve at startup. Power users edit
            // this field to use a different name.
            let key_name = cfg
                .api_key_name
                .clone()
                .unwrap_or_else(|| "MEM0_API_KEY".into());
            cfg.api_key_name = Some(key_name.clone());

            new_config.memory.extra = serde_json::to_value(&cfg)
                .map_err(|e| CliError::Config(format!("serialise mem0 extra: {e}")))?;

            secret_outcome = Some(prompt_and_store_api_key(ctx, &key_name, "Mem0").await?);
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

            cfg.account = prompt_optional(
                "Account ID (OpenViking tenant)",
                "default",
                cfg.account.as_deref(),
            )?;

            cfg.top_k = prompt_usize("top_k — max memories per recall", cfg.top_k.unwrap_or(5))?;

            // Default user-secret name `OPENVIKING_API_KEY`; not asked.
            // Persist the resolved name even when it equals the default
            // so opening `aura.json` shows where the key lives (same
            // rationale as Mem0 above).
            let key_name = cfg
                .api_key_name
                .clone()
                .unwrap_or_else(|| "OPENVIKING_API_KEY".into());
            cfg.api_key_name = Some(key_name.clone());

            new_config.memory.extra = serde_json::to_value(&cfg)
                .map_err(|e| CliError::Config(format!("serialise openviking extra: {e}")))?;

            // OpenViking auth is optional (server-controlled), so just ask
            // unconditionally — the inline prompt already says "press
            // enter to skip", and `prompt_and_store_api_key`'s Skipped
            // outcome surfaces a hint for setting it later. Don't try to
            // guess from the endpoint URL.
            secret_outcome = Some(prompt_and_store_api_key(ctx, &key_name, "OpenViking").await?);
        }
    }

    new_config.write_to_file(&target).await?;

    let mut human = format!(
        "memory set up: enabled={} provider={}\nwrote {}",
        new_config.memory.enabled,
        provider_label(new_config.memory.provider),
        target.display(),
    );
    let summary_line = secret_outcome.as_ref().map(SecretWriteOutcome::human);
    if let Some(line) = &summary_line {
        human.push_str("\n\n");
        human.push_str(line);
    }
    human.push_str(RESTART_HINT);
    Ok(CommandOutput {
        human,
        data: Some(json!({
            "enabled": new_config.memory.enabled,
            "provider": provider_label(new_config.memory.provider),
            "written_to": target.display().to_string(),
            "requires_restart": true,
            "api_key": summary_line,
        })),
    })
}

/// Outcome of the inline API-key prompt inside the setup wizard. Drives
/// the one-line summary the operator sees at the end of the flow.
enum SecretWriteOutcome {
    /// User typed a value; it was stored in the vault.
    Stored { name: String },
    /// User pressed Enter and there was already a value in the vault
    /// (or env var) — we left it alone.
    Kept { name: String },
    /// User pressed Enter and there was no existing value — point them
    /// at `aura secret add` so they can set it later.
    Skipped { name: String },
}

impl SecretWriteOutcome {
    fn human(&self) -> String {
        match self {
            Self::Stored { name } => format!("Stored {name} in vault."),
            Self::Kept { name } => format!("Kept existing {name} (vault or env)."),
            Self::Skipped { name } => format!(
                "Run `aura secret add {name}` to provide the API key \
                 (or set the {name} env var)."
            ),
        }
    }
}

/// Prompt the operator for an API-key value, masked, and store it at
/// `user_env.<name>` — the same vault path `aura secret add` writes to,
/// so the value round-trips with the existing secret management surface.
///
/// Empty input leaves the vault untouched. The bracketed label hints
/// whether a key is already present so the operator knows whether
/// pressing Enter keeps a prior value or skips entirely.
async fn prompt_and_store_api_key(
    ctx: &CommandContext,
    name: &str,
    provider_label: &str,
) -> Result<SecretWriteOutcome> {
    let vault = ctx.secret_vault.as_ref().ok_or_else(|| {
        CliError::Config("secret vault unavailable — run from the workspace root".into())
    })?;
    let vault_key = format!("{}{name}", aura_security::USER_SECRET_PREFIX);
    let already_set = matches!(vault.get_secret(&vault_key).await, Ok(Some(_)));

    let label = if already_set {
        format!("{provider_label} API key (already set; press enter to keep, or type to replace)")
    } else {
        format!("{provider_label} API key (stored as {name}; press enter to skip)")
    };
    let value = read_masked_password(&format!("{label}: "))?;
    if value.is_empty() {
        return Ok(if already_set {
            SecretWriteOutcome::Kept {
                name: name.to_string(),
            }
        } else {
            SecretWriteOutcome::Skipped {
                name: name.to_string(),
            }
        });
    }
    vault
        .store_secret(&vault_key, value.as_bytes())
        .await
        .map_err(|e| CliError::Config(format!("vault store {vault_key}: {e}")))?;
    Ok(SecretWriteOutcome::Stored {
        name: name.to_string(),
    })
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
                let name = cfg.api_key_name.as_deref().unwrap_or("MEM0_API_KEY");
                return Err(CliError::Config(format!(
                    "mem0 API key missing — run `aura secret add {name}` (or set the {name} env var)"
                )));
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
// prompt helpers for the detailed setup wizard
// ---------------------------------------------------------------------------

/// Prompt for an optional `String` field. The bracketed default is the
/// **current** value if one is set, otherwise `default_when_unset`. An empty
/// input returns `None` so the typed default kicks in and the field elides
/// from JSON (`skip_serializing_if`).
fn prompt_optional(
    label: &str,
    default_when_unset: &str,
    current: Option<&str>,
) -> Result<Option<String>> {
    let shown = current.unwrap_or(default_when_unset);
    let v = prompt_with_default(label, shown)?;
    if v.is_empty() || v == default_when_unset {
        Ok(None)
    } else {
        Ok(Some(v))
    }
}

/// Prompt for an `Option<bool>`. Empty input keeps the typed default
/// (returns `None`); `true`/`false` (or `yes`/`no` / `y`/`n` / `1`/`0`) is
/// stored only when it differs from `default_when_unset`.
fn prompt_bool(label: &str, default_when_unset: bool) -> Result<Option<bool>> {
    let shown = if default_when_unset { "true" } else { "false" };
    let v = prompt_with_default(&format!("{label} [true/false]"), shown)?;
    let parsed = match v.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "y" | "1" => true,
        "false" | "no" | "n" | "0" => false,
        other => {
            return Err(CliError::Config(format!(
                "unrecognised boolean {other:?} (expected true/false)"
            )));
        }
    };
    if parsed == default_when_unset {
        Ok(None)
    } else {
        Ok(Some(parsed))
    }
}

/// Prompt for an `Option<usize>`. Empty input keeps the typed default
/// (returns `None`); a parsed value is stored only when it differs from
/// `default_when_unset`.
fn prompt_usize(label: &str, default_when_unset: usize) -> Result<Option<usize>> {
    let shown = default_when_unset.to_string();
    let v = prompt_with_default(label, &shown)?;
    let parsed: usize = v
        .parse()
        .map_err(|_| CliError::Config(format!("invalid integer {v:?}")))?;
    if parsed == default_when_unset {
        Ok(None)
    } else {
        Ok(Some(parsed))
    }
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
}
