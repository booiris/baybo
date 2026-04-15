use crate::AuraConfig;
use crate::channels::ChannelsConfig;
use crate::cost::CostConfig;
use crate::error::{ConfigError, ValidationError};
use crate::llm::LlmConfig;
use crate::sandbox::SandboxConfig;
use crate::session::SessionConfig;
use crate::tools::ToolsConfig;
use crate::trace::TraceConfig;
use crate::workspace::WorkspaceConfig;

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
        validate_workspace(&self.workspace, &mut errors);
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

fn validate_workspace(workspace: &WorkspaceConfig, errors: &mut Vec<ValidationError>) {
    if workspace.path.trim().is_empty() {
        errors.push(ValidationError::new("workspace.path", "must be non-empty"));
    }
}

/// Cross-section consistency rules. Runs after per-section validation so that
/// field-level errors surface first and cross-section rules can assume each
/// section is internally coherent.
fn validate_cross_section(config: &AuraConfig, errors: &mut Vec<ValidationError>) {
    validate_encryption_key_source(&config.security, errors);
    validate_llm_secret_source(&config.llm, errors);
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
