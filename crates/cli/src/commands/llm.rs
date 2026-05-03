use aura_llm::providers::openai_subscription::{
    self as openai_subscription, DEFAULT_BASE_URL, DeviceCode, PROVIDER_NAME, VAULT_KEY_TOKENS,
    VaultTokenStore, device_code_login, pkce_login, revoke,
};
use aura_llm::{LlmProviderConfig, LlmProviderRegistry};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use crate::cli::{LlmAuthCmd, LlmCmd};
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: LlmCmd) -> Result<CommandOutput> {
    match cmd {
        LlmCmd::Status => status(ctx),
        LlmCmd::Models { live, provider } => {
            if live {
                // clap's required_if_eq enforces this — defensive double-check.
                let provider = provider
                    .ok_or_else(|| CliError::Manager("--live requires --provider <id>".into()))?;
                models_live(ctx, &provider).await
            } else {
                models()
            }
        }
        LlmCmd::Probe => probe(ctx).await,
        LlmCmd::Auth(auth) => auth_dispatch(ctx, auth).await,
    }
}

async fn auth_dispatch(ctx: &CommandContext, cmd: LlmAuthCmd) -> Result<CommandOutput> {
    match cmd {
        LlmAuthCmd::Login {
            provider,
            device_code,
        } => auth_login(ctx, &provider, device_code).await,
        LlmAuthCmd::Status { provider } => auth_status(ctx, &provider).await,
        LlmAuthCmd::Logout { provider } => auth_logout(ctx, &provider).await,
    }
}

fn require_openai_subscription(provider: &str) -> Result<()> {
    if provider != PROVIDER_NAME {
        return Err(CliError::Manager(format!(
            "`aura llm auth` currently only supports `--provider {PROVIDER_NAME}`, got \
             `{provider}`. API-key providers (openai, anthropic, gemini, minimax) read their \
             credential from the environment — see `aura llm models` for the full list."
        )));
    }
    Ok(())
}

fn require_vault(ctx: &CommandContext) -> Result<VaultTokenStore> {
    let vault = ctx.secret_vault.as_ref().ok_or_else(|| {
        CliError::Manager(
            "openai-subscription auth requires a SecretVault — make sure the gateway has \
             been started at least once so the vault and master key are initialised, then re-run \
             this command from the running install."
                .into(),
        )
    })?;
    Ok(VaultTokenStore::new(vault.clone()))
}

async fn auth_login(
    ctx: &CommandContext,
    provider: &str,
    device_code: bool,
) -> Result<CommandOutput> {
    require_openai_subscription(provider)?;
    let store = require_vault(ctx)?;

    let bundle = if device_code {
        device_code_login(|code: &DeviceCode| {
            // Print the code prominently to stdout so the user can copy it.
            println!(
                "\nopen this URL in any browser, sign in, and enter the code:\n  url:  {}\n  code: {}\n",
                code.verification_url, code.user_code
            );
        })
        .await
        .map_err(|e| CliError::Manager(format!("openai-subscription device-code login: {e}")))?
    } else {
        pkce_login(|url: &str| {
            // Try to open the URL in the default browser; fall back to printing
            // it. Errors from the open crate are non-fatal — we'd rather show
            // the URL than refuse to log in just because we couldn't spawn a
            // browser.
            println!("\nopen this URL to sign in (a browser may launch automatically):\n  {url}\n");
            Ok(())
        })
        .await
        .map_err(|e| CliError::Manager(format!("openai-subscription PKCE login: {e}")))?
    };

    store
        .save(&bundle)
        .await
        .map_err(|e| CliError::Manager(format!("openai-subscription vault save: {e}")))?;

    let value = json!({
        "provider": provider,
        "vault_key": VAULT_KEY_TOKENS,
        "email": bundle.email(),
        "plan_type": bundle.plan_type(),
        "expires_at": bundle.expires_at,
        "account_id": bundle.account_id,
    });
    let human = format!(
        "signed in as {} (plan: {})\ntoken stored at vault://{}",
        bundle.email().unwrap_or_else(|| "<unknown>".into()),
        bundle.plan_type().unwrap_or_else(|| "<unknown>".into()),
        VAULT_KEY_TOKENS,
    );
    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}

async fn auth_status(ctx: &CommandContext, provider: &str) -> Result<CommandOutput> {
    require_openai_subscription(provider)?;
    let store = require_vault(ctx)?;
    let bundle = store
        .load()
        .await
        .map_err(|e| CliError::Manager(format!("openai-subscription vault load: {e}")))?;

    let configured_base = ctx
        .config
        .llm
        .base_url
        .clone()
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let unsafe_env = openai_subscription::UNSAFE_BASE_URL_ENV_VAR;
    let unsafe_override_active = std::env::var(unsafe_env).is_ok();
    let endpoint_line = if configured_base == DEFAULT_BASE_URL {
        format!(
            "endpoint: {DEFAULT_BASE_URL}/codex/responses (default; Codex Responses only — \
             token is never sent to api.openai.com)"
        )
    } else if unsafe_override_active {
        format!(
            "endpoint: {configured_base}/codex/responses (UNSAFE OVERRIDE — {unsafe_env} is set, \
             so the bearer is being sent to a non-OpenAI host. Default is {DEFAULT_BASE_URL})"
        )
    } else {
        format!(
            "endpoint: {configured_base}/codex/responses (NON-DEFAULT and host is NOT on the \
             allowlist — provider construction will fail until you either revert this override \
             or set {unsafe_env}=1)"
        )
    };

    match bundle {
        None => Ok(CommandOutput {
            human: format!(
                "not signed in.  run: aura llm auth login --provider openai-subscription\n{endpoint_line}"
            ),
            data: Some(json!({
                "signed_in": false,
                "endpoint_default": configured_base == DEFAULT_BASE_URL,
            })),
        }),
        Some(b) => {
            let expires = DateTime::<Utc>::from_timestamp(b.expires_at, 0)
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| format!("epoch {}", b.expires_at));
            let now = Utc::now().timestamp();
            let near_expiry = b.is_near_expiry(60);
            let human = format!(
                "signed in as {}\nplan:     {}\naccount:  {}\nexpires:  {} ({})\n{endpoint_line}",
                b.email().unwrap_or_else(|| "<unknown>".into()),
                b.plan_type().unwrap_or_else(|| "<unknown>".into()),
                b.account_id.clone().unwrap_or_else(|| "<personal>".into()),
                expires,
                if near_expiry {
                    "near-expiry — next call will refresh"
                } else {
                    "valid"
                },
            );
            let data = json!({
                "signed_in": true,
                "email": b.email(),
                "plan_type": b.plan_type(),
                "account_id": b.account_id,
                "expires_at": b.expires_at,
                "now": now,
                "near_expiry": near_expiry,
                "endpoint_default": configured_base == DEFAULT_BASE_URL,
            });
            Ok(CommandOutput {
                human,
                data: Some(data),
            })
        }
    }
}

async fn auth_logout(ctx: &CommandContext, provider: &str) -> Result<CommandOutput> {
    require_openai_subscription(provider)?;
    let store = require_vault(ctx)?;

    // Step 1: load the bundle so we have a refresh_token to revoke.
    // Vault load failure is fatal — we can't honestly say "logged out"
    // if we don't even know whether something was stored.
    let bundle_result = store
        .load()
        .await
        .map_err(|e| CliError::Manager(format!("openai-subscription vault load: {e}")))?;

    // Step 2: best-effort server-side revocation. Logged but never
    // fatal — the user's intent ("forget this token locally") is
    // satisfied as long as we manage the vault delete in step 3.
    let revoke_outcome = match &bundle_result {
        Some(b) => match revoke(&b.refresh_token).await {
            Ok(()) => "revoked",
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "openai-subscription logout: server-side revoke failed; continuing with \
                     local vault delete"
                );
                "revoke_failed"
            }
        },
        None => "no_token_to_revoke",
    };

    // Step 3: delete the local vault entry. Failure here IS fatal
    // because telling the user "logged out" while leaving a usable
    // token at rest would be a security lie.
    store
        .clear()
        .await
        .map_err(|e| CliError::Manager(format!("openai-subscription vault clear: {e}")))?;

    let human = match (bundle_result.is_some(), revoke_outcome) {
        (false, _) => {
            "no openai-subscription token was stored — local vault entry is already absent"
                .to_string()
        }
        (true, "revoked") => format!(
            "signed out of openai-subscription (server-side token revoked, local vault \
             entry {} cleared)",
            VAULT_KEY_TOKENS
        ),
        (true, _) => format!(
            "signed out locally (vault entry {} cleared) — server-side revoke failed; \
             check logs. The token will remain valid on OpenAI's side until it expires.",
            VAULT_KEY_TOKENS
        ),
    };
    Ok(CommandOutput {
        human,
        data: Some(json!({
            "provider": provider,
            "vault_key": VAULT_KEY_TOKENS,
            "had_local_token": bundle_result.is_some(),
            "server_revoke_outcome": revoke_outcome,
        })),
    })
}

fn status(ctx: &CommandContext) -> Result<CommandOutput> {
    let client = ctx
        .llm
        .as_ref()
        .ok_or_else(|| CliError::Manager("llm client not initialised".into()))?;
    let info = client.model_info();
    let value = json!({
        "provider": info.provider,
        "model": info.id,
        "context_window": info.context_window,
        "supports_tools": info.supports_tools,
        "supports_vision": info.supports_vision,
        "pricing": {
            "input_per_1m_tokens": info.pricing.input_per_1m_tokens,
            "output_per_1m_tokens": info.pricing.output_per_1m_tokens,
        }
    });
    let human = format!(
        "provider: {}\nmodel:    {}\ncontext:  {} tokens\ntools:    {}\nvision:   {}\npricing:  ${:.2}/1M in, ${:.2}/1M out",
        info.provider,
        info.id,
        info.context_window,
        info.supports_tools,
        info.supports_vision,
        info.pricing.input_per_1m_tokens,
        info.pricing.output_per_1m_tokens,
    );
    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}

fn models() -> Result<CommandOutput> {
    let registry = LlmProviderRegistry::with_default_providers();
    let catalog = registry.list_models();

    if catalog.is_empty() {
        return Ok(CommandOutput {
            human: "(no providers registered)".into(),
            data: Some(json!({ "providers": [] })),
        });
    }

    let data: Vec<Value> = catalog
        .iter()
        .map(|p| json!({ "provider": p.provider, "models": p.models }))
        .collect();

    let mut human = String::new();
    for entry in &catalog {
        human.push_str(&format!("{}:\n", entry.provider));
        if entry.models.is_empty() {
            human.push_str("  (catalog not advertised)\n");
        } else {
            for m in &entry.models {
                human.push_str(&format!("  {m}\n"));
            }
        }
    }

    Ok(CommandOutput {
        human: human.trim_end().to_string(),
        data: Some(json!({ "providers": data })),
    })
}

async fn models_live(ctx: &CommandContext, provider: &str) -> Result<CommandOutput> {
    let registry = LlmProviderRegistry::with_default_providers();
    // Build the same shape boot would build, so the factory's live_models()
    // sees its credentials. base_url + supports_vision pick up whatever the
    // operator configured in aura.json — if `provider` doesn't match the
    // configured one, base_url falls through to the factory default.
    let config = LlmProviderConfig {
        provider: provider.to_string(),
        api_key: ctx
            .config
            .llm
            .api_key_env
            .as_ref()
            .and_then(|env| std::env::var(env).ok()),
        base_url: if ctx.config.llm.provider == provider {
            ctx.config.llm.base_url.clone()
        } else {
            None
        },
        model: if ctx.config.llm.provider == provider {
            ctx.config.llm.model.clone()
        } else {
            // Live discovery doesn't use model — pass a placeholder so the
            // factory's create()-style validation paths don't get a panic.
            "unused".into()
        },
        supports_vision: None,
        vault: ctx.secret_vault.clone(),
    };
    let models = registry
        .list_live_models(&config)
        .await
        .map_err(|e| CliError::Manager(format!("live model discovery: {e}")))?;
    if models.is_empty() {
        return Ok(CommandOutput {
            human: format!("provider {provider}: (catalog endpoint returned no models)"),
            data: Some(json!({ "provider": provider, "models": [] })),
        });
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
    let mut human = format!("{provider} (live):\n");
    for m in &models {
        // Compact human row: `<id>  [ctx 200000]  (display name)`
        let ctx_label = m
            .context_window
            .map(|c| format!("  [ctx {c}]"))
            .unwrap_or_default();
        let name_label = m
            .display_name
            .as_ref()
            .filter(|n| n.as_str() != m.id)
            .map(|n| format!("  ({n})"))
            .unwrap_or_default();
        human.push_str(&format!("  {}{}{}\n", m.id, ctx_label, name_label));
    }
    Ok(CommandOutput {
        human: human.trim_end().to_string(),
        data: Some(json!({ "provider": provider, "models": data })),
    })
}

async fn probe(ctx: &CommandContext) -> Result<CommandOutput> {
    let client = ctx
        .llm
        .as_ref()
        .ok_or_else(|| CliError::Manager("llm client not initialised".into()))?;

    let report = client
        .probe()
        .await
        .map_err(|e| CliError::Manager(format!("llm probe: {e}")))?;

    let value = json!({
        "provider": report.provider,
        "model": report.model,
        "latency_ms": report.latency_ms,
        "tokens": {
            "input": report.tokens.input_tokens,
            "output": report.tokens.output_tokens,
        },
    });

    let human = format!(
        "ok  provider={}  model={}  latency={}ms  tokens={}/{} (in/out)",
        report.provider,
        report.model,
        report.latency_ms,
        report.tokens.input_tokens,
        report.tokens.output_tokens,
    );

    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}
