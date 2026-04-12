use std::collections::HashSet;

use crate::AuraConfig;
use crate::channels::ChannelsConfig;
use crate::cost::CostConfig;
use crate::error::{ConfigError, ValidationError};
use crate::llm::LlmConfig;
use crate::sandbox::SandboxConfig;
use crate::session::SessionConfig;
use crate::tools::{CapabilityConfig, McpTransportConfig, ToolsConfig, TrustLevelConfig};
use crate::trace::TraceConfig;

impl AuraConfig {
    /// Validate the config, collecting every violation. Returns `Ok(())` when
    /// the config is well-formed, or [`ConfigError::Validation`] with the list
    /// of problems otherwise.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut errors = Vec::new();
        validate_llm(&self.llm, &mut errors);
        validate_agent(self, &mut errors);
        validate_session(&self.session, &mut errors);
        validate_channels(&self.channels, &mut errors);
        validate_sandbox(&self.sandbox, &mut errors);
        validate_tools(&self.tools, &mut errors);
        validate_trace(&self.trace, &mut errors);
        validate_cost(&self.cost, &mut errors);
        validate_cross_section(self, &mut errors);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Validation(errors))
        }
    }
}

fn validate_llm(llm: &LlmConfig, errors: &mut Vec<ValidationError>) {
    if llm.provider.trim().is_empty() {
        errors.push(ValidationError::new("llm.provider", "must be non-empty"));
    }
    if llm.model.trim().is_empty() {
        errors.push(ValidationError::new("llm.model", "must be non-empty"));
    }
    if let Some(url) = &llm.base_url
        && !is_http_url(url)
    {
        errors.push(ValidationError::new(
            "llm.base_url",
            "must start with http:// or https://",
        ));
    }
    if let Some(fallback) = &llm.fallback_model
        && fallback.trim().is_empty()
    {
        errors.push(ValidationError::new(
            "llm.fallback_model",
            "must be non-empty when set",
        ));
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
    if agent.default_tool_timeout_ms < 100 {
        errors.push(ValidationError::new(
            "agent.default_tool_timeout_ms",
            "must be >= 100",
        ));
    }
    if agent.workspace_path.trim().is_empty() {
        errors.push(ValidationError::new(
            "agent.workspace_path",
            "must be non-empty",
        ));
    }

    let ctx = &agent.context;
    if ctx.max_tokens == 0 {
        errors.push(ValidationError::new(
            "agent.context.max_tokens",
            "must be >= 1",
        ));
    }
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
}

fn validate_session(session: &SessionConfig, errors: &mut Vec<ValidationError>) {
    if session.timeout_minutes == 0 {
        errors.push(ValidationError::new(
            "session.timeout_minutes",
            "must be >= 1",
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
    if let Some(http) = &channels.http {
        if !http.enabled {
            errors.push(ValidationError::new(
                "channels.http",
                "set enabled=true or omit the http section entirely",
            ));
        }
        if http.bind_address.trim().is_empty() {
            errors.push(ValidationError::new(
                "channels.http.bind_address",
                "must be non-empty",
            ));
        }
        if http.port == 0 {
            errors.push(ValidationError::new("channels.http.port", "must be > 0"));
        }
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

fn validate_sandbox(sandbox: &SandboxConfig, errors: &mut Vec<ValidationError>) {
    if sandbox.wasm.timeout_ms < 100 {
        errors.push(ValidationError::new(
            "sandbox.wasm.timeout_ms",
            "must be >= 100",
        ));
    }
    if sandbox.wasm.max_memory_bytes < 1_048_576 {
        errors.push(ValidationError::new(
            "sandbox.wasm.max_memory_bytes",
            "must be >= 1048576 (1 MB)",
        ));
    }
    if sandbox.wasm.max_fuel < 1_000 {
        errors.push(ValidationError::new(
            "sandbox.wasm.max_fuel",
            "must be >= 1000",
        ));
    }
}

fn validate_tools(tools: &ToolsConfig, errors: &mut Vec<ValidationError>) {
    if tools.default_timeout_ms < 100 {
        errors.push(ValidationError::new(
            "tools.default_timeout_ms",
            "must be >= 100",
        ));
    }

    let mut seen = HashSet::new();
    for (i, entry) in tools.mcp_servers.iter().enumerate() {
        let name_field = format!("tools.mcp_servers[{i}].name");
        if entry.name.trim().is_empty() {
            errors.push(ValidationError::new(&name_field, "must be non-empty"));
        } else if !seen.insert(entry.name.as_str()) {
            errors.push(ValidationError::new(
                &name_field,
                format!("duplicate MCP server name '{}'", entry.name),
            ));
        }

        match &entry.transport {
            McpTransportConfig::Stdio { command, .. } => {
                if command.trim().is_empty() {
                    errors.push(ValidationError::new(
                        format!("tools.mcp_servers[{i}].transport.command"),
                        "must be non-empty",
                    ));
                }
            }
            McpTransportConfig::Http { url, .. } => {
                if !is_http_url(url) {
                    errors.push(ValidationError::new(
                        format!("tools.mcp_servers[{i}].transport.url"),
                        "must start with http:// or https://",
                    ));
                }
            }
        }
    }
}

fn validate_trace(trace: &TraceConfig, errors: &mut Vec<ValidationError>) {
    if trace.auto_snapshot && trace.snapshot_interval == 0 {
        errors.push(ValidationError::new(
            "trace.snapshot_interval",
            "must be >= 1 when auto_snapshot is true",
        ));
    }
}

fn validate_cost(cost: &CostConfig, errors: &mut Vec<ValidationError>) {
    let limits = &cost.spending_limits;
    check_positive(
        limits.user_daily_usd,
        "cost.spending_limits.user_daily_usd",
        errors,
    );
    check_positive(
        limits.user_monthly_usd,
        "cost.spending_limits.user_monthly_usd",
        errors,
    );
    check_positive(
        limits.global_daily_usd,
        "cost.spending_limits.global_daily_usd",
        errors,
    );
    if let (Some(daily), Some(monthly)) = (limits.user_daily_usd, limits.user_monthly_usd)
        && daily > monthly
    {
        errors.push(ValidationError::new(
            "cost.spending_limits.user_daily_usd",
            "must be <= cost.spending_limits.user_monthly_usd",
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

/// Cross-section consistency rules. Runs after per-section validation so that
/// field-level errors surface first and cross-section rules can assume each
/// section is internally coherent.
fn validate_cross_section(config: &AuraConfig, errors: &mut Vec<ValidationError>) {
    validate_encryption_key_source(&config.security, errors);
    validate_llm_secret_source(&config.llm, errors);
    validate_mcp_hosts_against_network(config, errors);
    validate_trust_capability_matrix(&config.tools, errors);
}

fn validate_encryption_key_source(
    security: &crate::security::SecurityConfig,
    errors: &mut Vec<ValidationError>,
) {
    let file_set = security.encryption_key_file.is_some();
    let env_set = !security.encryption_key_env.trim().is_empty();
    if !file_set && !env_set {
        errors.push(ValidationError::new(
            "security.encryption_key",
            "either encryption_key_file or encryption_key_env must be set",
        ));
    }
}

/// If `llm.api_key` is explicitly present it must be non-empty. When absent,
/// runtime falls back to provider-specific env vars (resolved outside of
/// `validate()`), so we do not demand anything here.
fn validate_llm_secret_source(llm: &LlmConfig, errors: &mut Vec<ValidationError>) {
    if let Some(name) = &llm.api_key_env {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            errors.push(ValidationError::new(
                "llm.api_key_env",
                "must be non-empty when set (omit the field to fall back to env vars)",
            ));
        } else if !is_env_var_name(trimmed) {
            errors.push(ValidationError::new(
                "llm.api_key_env",
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

/// Every MCP Http URL must point to a host explicitly permitted by the sandbox
/// network policy, either via [`NetworkPolicyConfig::allowed_domains`] (suffix
/// match on domain labels) or [`NetworkPolicyConfig::allow_loopback`].
fn validate_mcp_hosts_against_network(config: &AuraConfig, errors: &mut Vec<ValidationError>) {
    for (i, entry) in config.tools.mcp_servers.iter().enumerate() {
        let McpTransportConfig::Http { url, .. } = &entry.transport else {
            continue;
        };
        let Some(host) = extract_host(url) else {
            continue; // field-level validation already rejected bad URL schemes
        };
        if config.sandbox.network.allow_loopback && is_loopback_host(host) {
            continue;
        }
        if !host_matches_allowlist(host, &config.sandbox.network.allowed_domains) {
            errors.push(ValidationError::new(
                format!("tools.mcp_servers[{i}].transport.url"),
                format!(
                    "host '{host}' is not covered by sandbox.network.allowed_domains \
                     (or allow_loopback)"
                ),
            ));
        }
    }
}

/// Trust ceilings: `Installed`-level MCP servers must not declare destructive
/// capabilities. Matches the governance rules laid out in `docs/modules/tools.md`.
fn validate_trust_capability_matrix(tools: &ToolsConfig, errors: &mut Vec<ValidationError>) {
    for (i, entry) in tools.mcp_servers.iter().enumerate() {
        if !matches!(entry.trust_level, TrustLevelConfig::Installed) {
            continue;
        }
        for cap in &entry.capabilities {
            let forbidden_name = match cap {
                CapabilityConfig::WriteWorkspace => Some("write_workspace"),
                CapabilityConfig::SpawnProcess => Some("spawn_process"),
                _ => None,
            };
            if let Some(name) = forbidden_name {
                errors.push(ValidationError::new(
                    format!("tools.mcp_servers[{i}].capabilities"),
                    format!(
                        "capability '{name}' is not permitted for trust_level=installed; \
                         set trust_level=trusted if the tool truly requires it"
                    ),
                ));
            }
        }
    }
}

fn extract_host(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let after_userinfo = rest.rsplit_once('@').map(|(_, h)| h).unwrap_or(rest);
    let host_port = after_userinfo
        .split_once(['/', '?', '#'])
        .map(|(h, _)| h)
        .unwrap_or(after_userinfo);
    if let Some(stripped) = host_port.strip_prefix('[') {
        // IPv6 literal: [addr]:port
        let end = stripped.find(']')?;
        return Some(&stripped[..end]);
    }
    Some(
        host_port
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(host_port),
    )
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// Suffix match on domain labels. `pattern = "example.com"` covers
/// `"example.com"` and `"*.example.com"` (any number of subdomain labels), but
/// not `"notexample.com"`.
fn host_matches_allowlist(host: &str, allowed: &[String]) -> bool {
    allowed.iter().any(|pattern| {
        let p = pattern.trim();
        if p.is_empty() {
            return false;
        }
        if host == p {
            return true;
        }
        host.len() > p.len() + 1
            && host.ends_with(p)
            && host.as_bytes()[host.len() - p.len() - 1] == b'.'
    })
}

fn check_positive(value: Option<f64>, field: &str, errors: &mut Vec<ValidationError>) {
    if let Some(v) = value
        && (!v.is_finite() || v <= 0.0)
    {
        errors.push(ValidationError::new(field, "must be > 0.0"));
    }
}

fn is_http_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}
