use std::collections::HashSet;
use std::path::Path;

use aura_model::MicroUsd;

use crate::AuraConfig;
use crate::browser::BrowserConfig;
use crate::channels::ChannelsConfig;
use crate::cost::CostConfig;
use crate::error::{ConfigError, ValidationError};
use crate::gateway::GatewayConfig;
use crate::llm::LlmEntry;
use crate::workspace::WorkspaceConfig;

impl AuraConfig {
    /// Validate the config, collecting every violation. Returns `Ok(())` when
    /// the config is well-formed, or [`ConfigError::Validation`] with the list
    /// of problems otherwise.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut errors = Vec::new();
        validate_llm_entries(&self.llm, &mut errors);
        validate_agent(self, &mut errors);
        validate_channels(&self.channels, &mut errors);
        validate_cost(&self.cost, &mut errors);
        validate_workspace(&self.workspace, &mut errors);
        validate_browser(&self.browser, &mut errors);
        validate_gateway(&self.gateway, &mut errors);
        validate_proxy(self.proxy.as_ref(), &mut errors);
        validate_cross_section(self, &mut errors);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Validation(errors))
        }
    }
}

fn validate_llm_entries(entries: &[LlmEntry], errors: &mut Vec<ValidationError>) {
    // An empty list is valid: a fresh install has no LLM configured
    // until `aura llm add` runs. Runtime paths that actually need an
    // LLM (boot's build_llm_client, agent loop) error there with a
    // pointer to `aura llm add`.
    let mut seen: HashSet<&str> = HashSet::new();
    for (i, entry) in entries.iter().enumerate() {
        let prefix = format!("llm[{i}]");
        if entry.name.as_str().trim().is_empty() {
            errors.push(ValidationError::new(
                format!("{prefix}.name"),
                "must be non-empty",
            ));
        } else if !seen.insert(entry.name.as_str()) {
            errors.push(ValidationError::new(
                format!("{prefix}.name"),
                format!("duplicate entry name {:?} in llm list", entry.name),
            ));
        }
        if entry.provider.trim().is_empty() {
            errors.push(ValidationError::new(
                format!("{prefix}.provider"),
                "must be non-empty",
            ));
        }
        if entry.model.trim().is_empty() {
            errors.push(ValidationError::new(
                format!("{prefix}.model"),
                "must be non-empty",
            ));
        }
        if let Some(url) = &entry.base_url
            && !is_http_url(url)
        {
            errors.push(ValidationError::new(
                format!("{prefix}.base_url"),
                "must start with http:// or https://",
            ));
        }
        validate_llm_secret_source(entry, &prefix, errors);
    }
}

fn validate_agent(config: &AuraConfig, errors: &mut Vec<ValidationError>) {
    let agent = &config.agent;
    if agent.max_iterations == 0 {
        errors.push(ValidationError::new("agent.max_iterations", "must be >= 1"));
    } else if agent.max_iterations > 1000 {
        errors.push(ValidationError::new(
            "agent.max_iterations",
            "must be <= 1000",
        ));
    }
    let ctx = &agent.context;
    if !ctx.compression_threshold.is_finite()
        || ctx.compression_threshold <= 0.0
        || ctx.compression_threshold > 1.0
    {
        errors.push(ValidationError::new(
            "agent.context.compression_threshold",
            "must be in the range (0.0, 1.0]",
        ));
    }
    if ctx.keep_recent == 0 {
        errors.push(ValidationError::new(
            "agent.context.keep_recent",
            "must be >= 1",
        ));
    }
    if agent.max_subagent_depth > 32 {
        errors.push(ValidationError::new(
            "agent.max_subagent_depth",
            "must be <= 32 (chains deeper than this stress the lineage walker and rarely reflect deliberate design)",
        ));
    }
    if agent.max_subagents_per_root == 0 {
        errors.push(ValidationError::new(
            "agent.max_subagents_per_root",
            "must be >= 1 (setting to 0 disables `spawn_subagent` entirely; if that's the intent, remove the tool from the registry instead)",
        ));
    } else if agent.max_subagents_per_root > 256 {
        errors.push(ValidationError::new(
            "agent.max_subagents_per_root",
            "must be <= 256 (caps higher than this rarely reflect deliberate design and trade safety for cost)",
        ));
    }
    if agent.max_concurrent_background_jobs == 0 {
        errors.push(ValidationError::new(
            "agent.max_concurrent_background_jobs",
            "must be >= 1 (0 would queue every background subagent forever)",
        ));
    } else if agent.max_concurrent_background_jobs > 256 {
        errors.push(ValidationError::new(
            "agent.max_concurrent_background_jobs",
            "must be <= 256 (a budget this large defeats the purpose of bounding concurrency)",
        ));
    }
}

fn validate_channels(channels: &ChannelsConfig, errors: &mut Vec<ValidationError>) {
    if channels.message_buffer_size == 0 {
        errors.push(ValidationError::new(
            "channels.message_buffer_size",
            "must be >= 1",
        ));
    } else if channels.message_buffer_size > 65_536 {
        errors.push(ValidationError::new(
            "channels.message_buffer_size",
            "must be <= 65536",
        ));
    }
    if let Some(tg) = &channels.telegram {
        if !tg.enabled {
            errors.push(ValidationError::new(
                "channels.telegram",
                "set enabled=true or omit the telegram section entirely",
            ));
        }
        if tg.bot_token_env.trim().is_empty() {
            errors.push(ValidationError::new(
                "channels.telegram.bot_token_env",
                "must be non-empty",
            ));
        }
    }
    if let Some(dc) = &channels.discord {
        if !dc.enabled {
            errors.push(ValidationError::new(
                "channels.discord",
                "set enabled=true or omit the discord section entirely",
            ));
        }
        if dc.bot_token_env.trim().is_empty() {
            errors.push(ValidationError::new(
                "channels.discord.bot_token_env",
                "must be non-empty",
            ));
        }
    }
}

fn validate_cost(cost: &CostConfig, errors: &mut Vec<ValidationError>) {
    let limits = &cost.spending_limits;
    check_positive(limits.daily_usd, "cost.spending_limits.daily_usd", errors);
    check_positive(
        limits.monthly_usd,
        "cost.spending_limits.monthly_usd",
        errors,
    );
    if let (Some(daily), Some(monthly)) = (limits.daily_usd, limits.monthly_usd)
        && daily > monthly
    {
        errors.push(ValidationError::new(
            "cost.spending_limits.daily_usd",
            "must be <= cost.spending_limits.monthly_usd",
        ));
    }

    if cost.rate_limit.max_requests == 0 {
        errors.push(ValidationError::new(
            "cost.rate_limit.max_requests",
            "must be >= 1",
        ));
    }
    if cost.rate_limit.window_secs == 0 {
        errors.push(ValidationError::new(
            "cost.rate_limit.window_secs",
            "must be >= 1",
        ));
    }
}

fn validate_workspace(workspace: &WorkspaceConfig, errors: &mut Vec<ValidationError>) {
    if workspace.path.trim().is_empty() {
        errors.push(ValidationError::new("workspace.path", "must be non-empty"));
    } else {
        require_absolute(&workspace.path, "workspace.path", errors);
    }
}

fn validate_browser(browser: &BrowserConfig, errors: &mut Vec<ValidationError>) {
    if let Some(p) = &browser.chrome_path
        && let Some(s) = p.to_str()
    {
        require_absolute(s, "browser.chrome_path", errors);
    }
    if let Some(p) = &browser.profile_dir
        && let Some(s) = p.to_str()
    {
        require_absolute(s, "browser.profile_dir", errors);
    }
}

fn validate_gateway(gateway: &GatewayConfig, errors: &mut Vec<ValidationError>) {
    if gateway.bind_address.trim().is_empty() {
        errors.push(ValidationError::new(
            "gateway.bind_address",
            "must be non-empty",
        ));
    }
    if gateway.port == 0 {
        errors.push(ValidationError::new("gateway.port", "must be > 0"));
    }
    if gateway.shutdown_grace_secs == 0 {
        errors.push(ValidationError::new(
            "gateway.shutdown_grace_secs",
            "must be >= 1",
        ));
    }
    for (i, origin) in gateway.cors_allowed_origins.iter().enumerate() {
        if origin.trim().is_empty() {
            errors.push(ValidationError::new(
                format!("gateway.cors_allowed_origins[{i}]"),
                "must be non-empty",
            ));
        }
    }
}

fn validate_proxy(proxy: Option<&crate::proxy::ProxyConfig>, errors: &mut Vec<ValidationError>) {
    let Some(proxy) = proxy else {
        return;
    };
    if proxy.url.trim().is_empty() {
        errors.push(ValidationError::new("proxy.url", "must be non-empty"));
    } else if !proxy.has_supported_scheme() {
        errors.push(ValidationError::new(
            "proxy.url",
            "must start with a supported scheme: http://, https://, socks5://, \
             socks5h://, socks4:// or socks4a://",
        ));
    }
}

/// Cross-section consistency rules. Runs after per-section validation so that
/// field-level errors surface first and cross-section rules can assume each
/// section is internally coherent.
fn validate_cross_section(config: &AuraConfig, errors: &mut Vec<ValidationError>) {
    validate_encryption_key_source(&config.security, errors);
    validate_default_llm(config, errors);
    validate_model_tiers(config, errors);
    validate_default_external_agent(config, errors);
}

/// Every `agent.model_tiers` target must name a real `llm` entry.
/// Without this the misconfig only surfaces at next startup in
/// `LlmClientPool::with_tier_map`, after a CLI/gateway mutator has
/// already persisted the broken config.
fn validate_model_tiers(config: &AuraConfig, errors: &mut Vec<ValidationError>) {
    if config.agent.model_tiers.is_empty() {
        return;
    }
    let names: Vec<&str> = config.llm.iter().map(|e| e.name.as_str()).collect();
    for (tier, name) in &config.agent.model_tiers {
        if !names.contains(&name.as_str()) {
            errors.push(ValidationError::new(
                "agent.model_tiers",
                format!(
                    "tier `{}` maps to `{}`, which is not a name in `llm`; existing names: [{}]",
                    tier.as_str(),
                    name.as_str(),
                    names.join(", "),
                ),
            ));
        }
    }
}

fn validate_default_external_agent(config: &AuraConfig, errors: &mut Vec<ValidationError>) {
    let enabled = config.external_agents.enabled_kinds();
    match (
        enabled.len(),
        &config.external_agents.default_external_agent,
    ) {
        // Zero or one enabled: default is optional. Single enabled
        // kind is implicitly the default.
        (0 | 1, _) => {}
        (_, Some(d)) => {
            if !enabled.contains(d) {
                let joined = enabled
                    .iter()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                errors.push(ValidationError::new(
                    "external_agents.default_external_agent",
                    format!(
                        "{:?} is not among the enabled external agents [{joined}]",
                        d.as_str(),
                    ),
                ));
            }
        }
        (_, None) => {
            let joined = enabled
                .iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            errors.push(ValidationError::new(
                "external_agents.default_external_agent",
                format!(
                    "multiple external agents are enabled ([{joined}]); set `default_external_agent` to one of them",
                ),
            ));
        }
    }
}

fn validate_default_llm(config: &AuraConfig, errors: &mut Vec<ValidationError>) {
    // Empty `default-llm` is fine when `llm` is empty too — a fresh
    // install has nothing to point at. Once any entry exists,
    // `default-llm` must name one of them.
    if config.llm.is_empty() {
        return;
    }
    if config.default_llm.as_str().trim().is_empty() {
        errors.push(ValidationError::new(
            "default-llm",
            format!(
                "must name one of the entries in `llm`: [{}]",
                config
                    .llm
                    .iter()
                    .map(|e| e.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        ));
        return;
    }
    if config.llm.iter().all(|e| e.name != config.default_llm) {
        errors.push(ValidationError::new(
            "default-llm",
            format!(
                "no entry named {:?} in `llm`; existing names: [{}]",
                config.default_llm,
                config
                    .llm
                    .iter()
                    .map(|e| e.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        ));
    }
}

fn validate_encryption_key_source(
    security: &crate::security::SecurityConfig,
    errors: &mut Vec<ValidationError>,
) {
    match &security.encryption_key_file {
        Some(file) => require_absolute(file, "security.encryption_key_file", errors),
        None => errors.push(ValidationError::new(
            "security.encryption_key_file",
            "must be set (run `aura setup` to mint a key under <workspace>/.key/encryption.key)",
        )),
    }
}

/// Reject relative paths so the value resolves identically regardless
/// of which cwd a subprocess inherits. Empty/whitespace-only is left
/// for the per-field non-empty check (it would otherwise produce two
/// confusing errors for the same field).
fn require_absolute(value: &str, field: &str, errors: &mut Vec<ValidationError>) {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return;
    }
    if !Path::new(trimmed).is_absolute() {
        errors.push(ValidationError::new(
            field,
            format!("must be an absolute path (got {trimmed:?})"),
        ));
    }
}

/// If `entry.api_key_env` is explicitly present it must name a valid
/// environment variable. When absent, runtime falls back to
/// provider-specific env vars (resolved outside of `validate()`), so we
/// do not demand anything here.
fn validate_llm_secret_source(entry: &LlmEntry, prefix: &str, errors: &mut Vec<ValidationError>) {
    if let Some(name) = &entry.api_key_env {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            errors.push(ValidationError::new(
                format!("{prefix}.api_key_env"),
                "must be non-empty when set (omit the field to fall back to env vars)",
            ));
        } else if !is_env_var_name(trimmed) {
            errors.push(ValidationError::new(
                format!("{prefix}.api_key_env"),
                "must be a valid environment variable name (letters, digits, underscores; \
                 not starting with a digit). Store the secret in the environment, not here.",
            ));
        }
    }
}

fn is_env_var_name(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn check_positive(value: Option<MicroUsd>, field: &str, errors: &mut Vec<ValidationError>) {
    if let Some(v) = value
        && v <= MicroUsd::ZERO
    {
        errors.push(ValidationError::new(field, "must be > 0.0"));
    }
}

fn is_http_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}
