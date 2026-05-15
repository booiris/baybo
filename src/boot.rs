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
    AgentConfig, AuraConfig, LlmEntry, RiskCheckConfig, SecurityConfig, WorkspaceConfig,
};
use aura_context::TokenBudget;
use aura_llm::credentials::resolve_api_key;
use aura_llm::{GuardedLlm, LlmCallGuard, LlmProviderConfig, LlmProviderRegistry};
use aura_security::{EncryptionKey, LeakDetector};
use aura_skills_assessor::AssessmentMode;
use aura_workspace::WorkspacePaths;
use aura_workspace::paths::{ENV_CONFIG_PATH, default_config_file};
use tracing::info;

// ---------------------------------------------------------------------------
// Loaders (perform I/O)
// ---------------------------------------------------------------------------

/// Resolve the config path from `AURA_CONFIG_PATH`, else
/// `<default_workspace_root>/config/aura.json`, else fall back to
/// `AuraConfig::default()`. An explicit `AURA_CONFIG_PATH` that points at a
/// missing file is a hard error — silent fallback would hide typos.
pub async fn load_config() -> anyhow::Result<AuraConfig> {
    let explicit = std::env::var(ENV_CONFIG_PATH).ok();
    let path = explicit
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(default_config_file);

    if !path.exists() {
        if explicit.is_some() {
            anyhow::bail!(
                "{ENV_CONFIG_PATH} points to {} but the file does not exist",
                path.display()
            );
        }
        info!(
            "no aura.json found at {}, using default configuration",
            path.display()
        );
        return Ok(AuraConfig::default());
    }

    info!(path = %path.display(), "loading configuration");
    AuraConfig::load_from_file(&path)
        .await
        .map_err(|e| anyhow::anyhow!("failed to load config from {}: {e}", path.display()))
}

/// Resolve the effective `aura.json` path, if any.
///
/// Same precedence as [`load_config`]: `AURA_CONFIG_PATH` first, then
/// `<default_workspace_root>/config/aura.json` (only if present). Returns
/// `None` when neither exists — callers running against
/// `AuraConfig::default()` have no path to write back to, and mutation
/// endpoints reject accordingly.
pub fn resolve_config_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var(ENV_CONFIG_PATH) {
        return Some(PathBuf::from(explicit));
    }
    let default = default_config_file();
    default.exists().then_some(default)
}

/// Build an [`Arc<GuardedLlm>`] from the `default-llm` entry of an
/// `AuraConfig`, resolving the api key through env then vault. The
/// returned handle is sealed: every `chat`/`chat_stream` runs through
/// `guard` first.
///
/// `registry` is borrowed by the caller so the same instance can be
/// reused for harvesting `all_known_pricings()` and constructing the
/// client — `LlmProviderRegistry::with_default_providers()` is cheap
/// but the runtime needs both anyway, and threading a single registry
/// makes the "one source of truth for providers" relationship
/// explicit at the call site.
///
/// `blob_store` is optional. When `Some`, the inner client is wired
/// with a `BlobFetcher` so vision-capable models actually receive
/// image bytes; without it, multimodal blocks degrade to a text stub
/// even on a model that claims `supports_vision: true`. Pass `None`
/// only for one-shot tooling (e.g. a `probe` subcommand) that never
/// sends multimodal content.
///
/// `guard` is the gate the resulting client will run before every
/// LLM call. Production wiring derives this from `CostManager`; CLI
/// `aura llm probe` and similar one-shot tools pass an
/// always-`Ok(())` closure so the probe isn't billed against anyone.
pub async fn build_llm_client(
    cfg: &AuraConfig,
    registry: &LlmProviderRegistry,
    blob_store: Option<std::sync::Arc<dyn aura_storage::BlobStore>>,
    vault: Option<std::sync::Arc<aura_security::SecretVault>>,
    guard: LlmCallGuard,
) -> anyhow::Result<std::sync::Arc<GuardedLlm>> {
    let entry = cfg.default_llm_entry().ok_or_else(|| {
        if cfg.llm.is_empty() {
            anyhow::anyhow!(
                "no LLM entries configured in aura.json — run `aura llm add` to register one"
            )
        } else {
            anyhow::anyhow!(
                "default-llm = {:?} does not match any entry in `llm` (existing: [{}])",
                cfg.default_llm,
                cfg.llm
                    .iter()
                    .map(|e| e.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        }
    })?;
    build_llm_client_for_entry(entry, registry, blob_store, vault, guard).await
}

/// Same wiring as [`build_llm_client`] but pinned to a specific
/// non-default entry. Used by CLI tooling (`probe`, live model
/// listing) and by the runtime when the active entry isn't the
/// default.
pub async fn build_llm_client_for_entry(
    entry: &LlmEntry,
    registry: &LlmProviderRegistry,
    blob_store: Option<std::sync::Arc<dyn aura_storage::BlobStore>>,
    vault: Option<std::sync::Arc<aura_security::SecretVault>>,
    guard: LlmCallGuard,
) -> anyhow::Result<std::sync::Arc<GuardedLlm>> {
    let api_key = resolve_api_key(
        &entry.name,
        &entry.provider,
        entry.api_key_env.as_deref(),
        vault.as_deref(),
    )
    .await;
    let blob_fetcher: Option<std::sync::Arc<dyn aura_llm::BlobFetcher>> = blob_store.map(|store| {
        std::sync::Arc::new(BlobStoreFetcher(store)) as std::sync::Arc<dyn aura_llm::BlobFetcher>
    });
    registry
        .create_client(
            &LlmProviderConfig {
                provider: entry.provider.clone(),
                api_key,
                base_url: entry.base_url.clone(),
                model: entry.model.clone(),
                supports_vision: entry.supports_vision,
                context_window: entry.context_window,
                pricing: entry.pricing,
                reasoning_effort: entry.reasoning_effort.clone(),
                vault,
            },
            blob_fetcher,
            guard,
        )
        .map_err(|e| anyhow::anyhow!("failed to build LLM client: {e}"))
}

/// Bridge `aura_storage::BlobStore` into `aura_llm::BlobFetcher`. Lives
/// here next to `build_llm_client` because both crates are framework-
/// agnostic and shouldn't know about each other — the application
/// boot layer is the only place that's allowed to glue them together.
struct BlobStoreFetcher(std::sync::Arc<dyn aura_storage::BlobStore>);

#[async_trait::async_trait]
impl aura_llm::BlobFetcher for BlobStoreFetcher {
    async fn fetch(&self, blob_id: &str) -> aura_llm::Result<Vec<u8>> {
        self.0
            .get(blob_id)
            .await
            .map_err(|e| aura_llm::LlmError::Transient(format!("blob fetch: {e}")))
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
    }
}

/// Path to the libsql database file, derived from the project root
/// (`workspace.path`). Storage always lives at `<root>/state/storage.db`;
/// the workspace root is itself the aura data directory (defaults to
/// `~/.aura` in release, `./.aura` in debug).
pub fn storage_db_path(cfg: &WorkspaceConfig) -> PathBuf {
    WorkspacePaths::new(PathBuf::from(&cfg.path)).storage_db()
}

pub fn to_token_budget(cfg: &aura_config::ContextConfig) -> TokenBudget {
    TokenBudget::new(cfg.max_tokens, cfg.compression_threshold)
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
    use aura_config::{AgentConfig, ContextConfig, SecurityConfig, WorkspaceConfig};

    #[test]
    fn execution_policy_maps_max_iterations() {
        let cfg = AgentConfig {
            max_iterations: 42,
            ..AgentConfig::default()
        };
        let policy = to_execution_policy(&cfg);
        assert_eq!(policy.max_iterations, 42);
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
    fn storage_db_path_is_under_workspace_state_dir() {
        let cfg = WorkspaceConfig {
            path: "/tmp/project".into(),
        };
        assert_eq!(
            storage_db_path(&cfg),
            std::path::PathBuf::from("/tmp/project/state/storage.db"),
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
