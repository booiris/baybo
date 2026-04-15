//! Bootstrap layer: `AuraConfig` → domain type translation.
//!
//! Everything in this module is a **pure mapping** from config to a domain
//! type, or a small **loader** that resolves a config reference (env var or
//! file path) into a live value. No `Arc` wiring, no channel allocation, no
//! actor spawning — those belong to `main.rs`.
//!
//! The split exists so the translation rules are in one place, covered by
//! unit tests, and decoupled from the rest of the bootstrap choreography.

use std::path::{Path, PathBuf};

use aura_agent::policy::ExecutionPolicy;
use aura_config::{
    AgentConfig, AuraConfig, LlmConfig, RiskCheckConfig, SecurityConfig, SessionConfig,
    ToolsConfig, WorkspaceConfig,
};
use aura_context::TokenBudget;
use aura_llm::{LlmClient, LlmProviderConfig, LlmProviderRegistry};
use aura_security::{EncryptionKey, LeakDetector};
use aura_skills_assessor::AssessmentMode;
use tracing::info;

// ---------------------------------------------------------------------------
// Loaders (perform I/O)
// ---------------------------------------------------------------------------

/// Resolve the config path from `AURA_CONFIG_PATH`, else `./aura.json`, else
/// fall back to `AuraConfig::default()`. An explicit `AURA_CONFIG_PATH` that
/// points at a missing file is a hard error — silent fallback would hide typos.
pub async fn load_config() -> anyhow::Result<AuraConfig> {
    let explicit = std::env::var("AURA_CONFIG_PATH").ok();
    let path = explicit
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("aura.json"));

    if !path.exists() {
        if explicit.is_some() {
            anyhow::bail!(
                "AURA_CONFIG_PATH points to {} but the file does not exist",
                path.display()
            );
        }
        info!("no aura.json found, using default configuration");
        return Ok(AuraConfig::default());
    }

    info!(path = %path.display(), "loading configuration");
    AuraConfig::load_from_file(&path)
        .await
        .map_err(|e| anyhow::anyhow!("failed to load config from {}: {e}", path.display()))
}

/// Build an `LlmClient` from `LlmConfig`, resolving the api key through env.
pub fn build_llm_client(cfg: &LlmConfig) -> anyhow::Result<LlmClient> {
    let registry = LlmProviderRegistry::with_default_providers();
    registry
        .create_client(&LlmProviderConfig {
            provider: cfg.provider.clone(),
            api_key: resolve_llm_api_key(cfg),
            base_url: cfg.base_url.clone(),
            model: cfg.model.clone(),
        })
        .map_err(|e| anyhow::anyhow!("failed to build LLM client: {e}"))
}

/// Resolve the LLM API key from the env var named by `api_key_env`, falling
/// back to provider-specific defaults (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`,
/// `GEMINI_API_KEY`). Returns `None` if nothing is set — the provider may
/// still accept it (e.g. local models) or error later.
pub fn resolve_llm_api_key(cfg: &LlmConfig) -> Option<String> {
    if let Some(env) = &cfg.api_key_env
        && let Ok(v) = std::env::var(env)
    {
        return Some(v);
    }
    match cfg.provider.as_str() {
        "openai" => std::env::var("OPENAI_API_KEY").ok(),
        "anthropic" => std::env::var("ANTHROPIC_API_KEY").ok(),
        "gemini" => std::env::var("GEMINI_API_KEY").ok(),
        _ => None,
    }
}

/// Load the 32-byte encryption key from file (hex-encoded) or env var (hex-encoded).
/// `validate()` guarantees at least one source is configured.
pub fn load_encryption_key(cfg: &SecurityConfig) -> anyhow::Result<EncryptionKey> {
    let hex = if let Some(path) = &cfg.encryption_key_file {
        std::fs::read_to_string(Path::new(path))
            .map_err(|e| anyhow::anyhow!("failed to read encryption_key_file {path}: {e}"))?
            .trim()
            .to_string()
    } else if !cfg.encryption_key_env.is_empty() {
        std::env::var(&cfg.encryption_key_env).map_err(|_| {
            anyhow::anyhow!(
                "encryption key env var '{}' not set",
                cfg.encryption_key_env
            )
        })?
    } else {
        anyhow::bail!("no encryption key source configured");
    };

    let bytes = hex::decode(hex.trim())
        .map_err(|e| anyhow::anyhow!("encryption key is not valid hex: {e}"))?;
    if bytes.len() != 32 {
        anyhow::bail!("encryption key must be 32 bytes, got {}", bytes.len());
    }
    EncryptionKey::new(bytes).map_err(|e| anyhow::anyhow!("invalid encryption key: {e}"))
}

// ---------------------------------------------------------------------------
// Pure mappings (no I/O, no Arc, no allocation beyond the returned value)
// ---------------------------------------------------------------------------

pub fn to_execution_policy(cfg: &AgentConfig) -> ExecutionPolicy {
    ExecutionPolicy {
        max_iterations: cfg.max_iterations,
        default_tool_timeout_ms: cfg.default_tool_timeout_ms,
    }
}

/// Path to the libsql database file, derived from the project root
/// (`workspace.path`). Storage always lives at `<root>/storage.db`; the
/// workspace root is itself the aura data directory (defaults to
/// `~/.aura` in release, `./.aura` in debug).
pub fn storage_db_path(cfg: &WorkspaceConfig) -> PathBuf {
    PathBuf::from(&cfg.path).join("storage.db")
}

pub fn to_token_budget(cfg: &aura_config::ContextConfig) -> TokenBudget {
    TokenBudget::new(cfg.max_tokens, cfg.compression_threshold)
}

pub fn to_session_timeout(cfg: &SessionConfig) -> chrono::Duration {
    chrono::Duration::minutes(cfg.timeout_minutes as i64)
}

pub fn to_tool_timeout(cfg: &ToolsConfig) -> std::time::Duration {
    std::time::Duration::from_millis(cfg.default_timeout_ms)
}

pub fn to_assessment_mode(cfg: RiskCheckConfig) -> AssessmentMode {
    match cfg {
        RiskCheckConfig::Off => AssessmentMode::Off,
        RiskCheckConfig::Primary => AssessmentMode::Primary,
        RiskCheckConfig::Full => AssessmentMode::Full,
    }
}

pub fn build_leak_detector(cfg: &SecurityConfig) -> LeakDetector {
    if cfg.leak_detection_enabled {
        LeakDetector::with_default_rules()
    } else {
        LeakDetector::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_config::{
        AgentConfig, ContextConfig, SecurityConfig, SessionConfig, ToolsConfig, WorkspaceConfig,
    };

    #[test]
    fn execution_policy_maps_both_fields() {
        let cfg = AgentConfig {
            max_iterations: 42,
            default_tool_timeout_ms: 7_500,
            ..AgentConfig::default()
        };
        let policy = to_execution_policy(&cfg);
        assert_eq!(policy.max_iterations, 42);
        assert_eq!(policy.default_tool_timeout_ms, 7_500);
    }

    #[test]
    fn session_timeout_converts_minutes_to_chrono() {
        let cfg = SessionConfig {
            timeout_minutes: 15,
            ..SessionConfig::default()
        };
        assert_eq!(to_session_timeout(&cfg), chrono::Duration::minutes(15));
    }

    #[test]
    fn tool_timeout_converts_millis() {
        let cfg = ToolsConfig {
            default_timeout_ms: 250,
        };
        assert_eq!(to_tool_timeout(&cfg), std::time::Duration::from_millis(250));
    }

    #[test]
    fn token_budget_carries_max() {
        let cfg = ContextConfig {
            max_tokens: 50_000,
            compression_threshold: 0.5,
            keep_recent: 10,
        };
        assert_eq!(to_token_budget(&cfg).max_tokens(), 50_000);
    }

    #[test]
    fn storage_db_path_is_under_workspace_root() {
        let cfg = WorkspaceConfig {
            path: "/tmp/project".into(),
        };
        assert_eq!(
            storage_db_path(&cfg),
            std::path::PathBuf::from("/tmp/project/storage.db"),
        );
    }

    #[test]
    fn leak_detector_builds_for_both_flag_values() {
        // Behavioral-equivalence checks would couple the test to the default
        // rule set; instead verify only that both branches produce a usable
        // detector (the real difference is covered by integration-level runs).
        let off = SecurityConfig {
            leak_detection_enabled: false,
            ..SecurityConfig::default()
        };
        let on = SecurityConfig {
            leak_detection_enabled: true,
            ..SecurityConfig::default()
        };
        let _ = build_leak_detector(&off);
        let _ = build_leak_detector(&on);
    }
}
