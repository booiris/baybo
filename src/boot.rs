//! Bootstrap layer: `BayboConfig` → domain type translation.
//!
//! Everything in this module is a **pure mapping** from config to a domain
//! type, or a small **loader** that resolves a config reference (env var or
//! file path) into a live value. No `Arc` wiring, no channel allocation, no
//! actor spawning — those belong to `main.rs`.
//!
//! The split exists so the translation rules are in one place, covered by
//! unit tests, and decoupled from the rest of the bootstrap choreography.

use std::path::{Path, PathBuf};

use baybo_config::{BayboConfig, LlmEntry, RiskCheckConfig, SecurityConfig, WorkspaceConfig};
use baybo_llm::credentials::resolve_api_key;
use baybo_llm::{BillableLlm, CostHooks, LlmProviderConfig, LlmProviderRegistry};
use baybo_security::{EncryptionKey, LeakDetector};
use baybo_skills_assessor::AssessmentMode;
use baybo_workspace::WorkspacePaths;
use baybo_workspace::paths::{ENV_CONFIG_PATH, default_config_file};
use tracing::info;

// ---------------------------------------------------------------------------
// Loaders (perform I/O)
// ---------------------------------------------------------------------------

/// Resolve the config path from `BAYBO_CONFIG_PATH`, else
/// `<default_workspace_root>/config/baybo.json`, else fall back to
/// `BayboConfig::default()`. An explicit `BAYBO_CONFIG_PATH` that points at a
/// missing file is a hard error — silent fallback would hide typos.
pub async fn load_config() -> anyhow::Result<BayboConfig> {
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
            "no baybo.json found at {}, using default configuration",
            path.display()
        );
        return Ok(BayboConfig::default());
    }

    info!(path = %path.display(), "loading configuration");
    BayboConfig::load_from_file(&path)
        .await
        .map_err(|e| anyhow::anyhow!("failed to load config from {}: {e}", path.display()))
}

/// Resolve the effective `baybo.json` path, if any.
///
/// Same precedence as [`load_config`]: `BAYBO_CONFIG_PATH` first, then
/// `<default_workspace_root>/config/baybo.json` (only if present). Returns
/// `None` when neither exists — callers running against
/// `BayboConfig::default()` have no path to write back to, and mutation
/// endpoints reject accordingly.
pub fn resolve_config_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var(ENV_CONFIG_PATH) {
        return Some(PathBuf::from(explicit));
    }
    let default = default_config_file();
    default.exists().then_some(default)
}

/// Build an [`Arc<BillableLlm>`] from the `default-llm` entry of an
/// `BayboConfig`, resolving the api key through env then vault. The
/// returned handle is sealed: every `chat`/`chat_stream` runs through
/// `guard` first.
///
/// `registry` is borrowed by the caller so a single instance is the one
/// source of truth for providers — `LlmProviderRegistry::with_default_providers()`
/// is cheap, but threading one registry through every `create()` makes
/// that relationship explicit at the call site.
///
/// `blob_store` is optional. When `Some`, the inner client is wired
/// with a `BlobFetcher` so vision-capable models actually receive
/// image bytes; without it, multimodal blocks degrade to a text stub
/// even on a model that claims `supports_vision: true`. Pass `None`
/// only for one-shot tooling (e.g. a `probe` subcommand) that never
/// sends multimodal content.
///
/// `billing` carries the gate the resulting client runs before every
/// LLM call and the recorder it runs after. Production wiring derives
/// this from `CostManager` (`baybo_cost::cost_hooks`); CLI `baybo llm
/// probe` and similar one-shot tools pass [`CostHooks::passthrough`] so
/// the probe isn't gated or billed against anyone.
pub async fn build_llm_client(
    cfg: &BayboConfig,
    registry: &LlmProviderRegistry,
    blob_store: Option<std::sync::Arc<dyn baybo_store::BlobStore>>,
    vault: Option<std::sync::Arc<baybo_security::SecretVault>>,
    billing: CostHooks,
) -> anyhow::Result<std::sync::Arc<BillableLlm>> {
    let entry = cfg.default_llm_entry().ok_or_else(|| {
        if cfg.llm.is_empty() {
            anyhow::anyhow!(
                "no LLM entries configured in baybo.json — run `baybo llm add` to register one"
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
    let proxy = proxy_settings(cfg);
    build_llm_client_for_entry(entry, registry, blob_store, vault, billing, proxy).await
}

/// Same wiring as [`build_llm_client`] but pinned to a specific
/// non-default entry. Used by CLI tooling (`probe`, live model
/// listing) and by the runtime when the active entry isn't the
/// default.
pub async fn build_llm_client_for_entry(
    entry: &LlmEntry,
    registry: &LlmProviderRegistry,
    blob_store: Option<std::sync::Arc<dyn baybo_store::BlobStore>>,
    vault: Option<std::sync::Arc<baybo_security::SecretVault>>,
    billing: CostHooks,
    proxy: Option<baybo_security::http::ProxySettings>,
) -> anyhow::Result<std::sync::Arc<BillableLlm>> {
    let api_key = resolve_api_key(
        entry.name.as_str(),
        &entry.provider,
        entry.api_key_env.as_deref(),
        vault.as_deref(),
    )
    .await;
    let blob_fetcher: Option<std::sync::Arc<dyn baybo_llm::BlobFetcher>> =
        blob_store.map(|store| {
            std::sync::Arc::new(BlobStoreFetcher(store))
                as std::sync::Arc<dyn baybo_llm::BlobFetcher>
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
                proxy,
            },
            blob_fetcher,
            billing,
        )
        .map_err(|e| anyhow::anyhow!("failed to build LLM client: {e}"))
}

/// Bridge `baybo_store::BlobStore` into `baybo_llm::BlobFetcher`. Lives
/// here next to `build_llm_client` because both crates are framework-
/// agnostic and shouldn't know about each other — the application
/// boot layer is the only place that's allowed to glue them together.
struct BlobStoreFetcher(std::sync::Arc<dyn baybo_store::BlobStore>);

#[async_trait::async_trait]
impl baybo_llm::BlobFetcher for BlobStoreFetcher {
    async fn fetch(&self, blob_id: &str) -> baybo_llm::Result<Vec<u8>> {
        self.0
            .get(blob_id)
            .await
            .map_err(|e| baybo_llm::LlmError::Transient(format!("blob fetch: {e}")))
    }
}

/// Load the 32-byte encryption key from `security.encryption_key_file`
/// (hex-encoded). `validate()` guarantees the field is set and absolute;
/// a missing or malformed file is a hard error — there is no dev-key
/// fallback. `baybo setup` mints the file on first run.
pub fn load_encryption_key(cfg: &SecurityConfig) -> anyhow::Result<EncryptionKey> {
    let path = cfg
        .encryption_key_file
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("security.encryption_key_file is not set"))?;
    let hex = std::fs::read_to_string(Path::new(path))
        .map_err(|e| anyhow::anyhow!("failed to read encryption_key_file {path}: {e}"))?;
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

/// Path to the libsql database file, derived from the project root
/// (`workspace.path`). Storage always lives at `<root>/state/storage.db`;
/// the workspace root is itself the baybo data directory (defaults to
/// `~/.baybo` in release, `./.baybo` in debug).
pub fn storage_db_path(cfg: &WorkspaceConfig) -> PathBuf {
    WorkspacePaths::new(PathBuf::from(&cfg.path)).storage_db()
}

/// Map the optional `proxy` config into the runtime [`ProxySettings`] every
/// HTTP client + spawned child threads through. `None` (no `proxy` block) =
/// direct connections, today's behavior.
pub fn proxy_settings(cfg: &BayboConfig) -> Option<baybo_security::http::ProxySettings> {
    cfg.proxy
        .as_ref()
        .map(|p| baybo_security::http::ProxySettings {
            url: p.url.clone(),
            no_proxy: p.no_proxy.clone(),
        })
}

pub fn to_assessment_mode(cfg: RiskCheckConfig) -> AssessmentMode {
    match cfg {
        RiskCheckConfig::Off => AssessmentMode::Off,
        RiskCheckConfig::Primary => AssessmentMode::Primary,
        RiskCheckConfig::Full => AssessmentMode::Full,
    }
}

/// Map the config-layer sandbox mode onto the tool-layer one. The two are kept
/// separate so `baybo-tools` needn't depend on `baybo-config`; this boot/wiring
/// layer (which has both) is the one place that bridges them. Shared by initial
/// wiring ([`crate::runtime`]) and hot-reload ([`crate::reload`]) so both map
/// identically.
pub fn to_bash_mode(mode: baybo_config::SandboxMode) -> baybo_tools::builtin::BashSandboxMode {
    use baybo_tools::builtin::BashSandboxMode;
    match mode {
        baybo_config::SandboxMode::None => BashSandboxMode::None,
        baybo_config::SandboxMode::Sandboxed => BashSandboxMode::Sandboxed,
        baybo_config::SandboxMode::Auto => BashSandboxMode::Auto,
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
    use baybo_config::{SecurityConfig, WorkspaceConfig};

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
    fn proxy_settings_maps_config_to_runtime() {
        let cfg = BayboConfig {
            proxy: Some(baybo_config::ProxyConfig {
                url: "socks5://127.0.0.1:1080".into(),
                no_proxy: Some(vec![".internal".into()]),
            }),
            ..Default::default()
        };
        let settings = proxy_settings(&cfg).expect("Some when proxy configured");
        assert_eq!(settings.url, "socks5://127.0.0.1:1080");
        assert_eq!(settings.no_proxy, Some(vec![".internal".to_string()]));
        assert!(
            proxy_settings(&BayboConfig::default()).is_none(),
            "no proxy block ⇒ None ⇒ direct connections"
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
