//! Config hot-reload trait + result types.
//!
//! The trait lives here so [`crate::server::AdminState`] can hold an
//! `Arc<dyn ConfigReloader>`; the concrete implementation lives in the
//! bin crate, which needs the application boot layer to rebuild the LLM
//! pool. See `docs/config-hot-reload.md`.

use async_trait::async_trait;
use aura_config::AuraConfig;
use serde::Serialize;
use utoipa::ToSchema;

/// Outcome of a successful reload, surfaced to the operator (HTTP
/// response + logs).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ReloadOutcome {
    /// Model id of the (possibly new) default entry now serving turns.
    pub active_model: String,
    /// `default-llm` entry name after the reload.
    pub default_entry: String,
    /// All entry names present in the rebuilt pool.
    pub entries: Vec<String>,
    /// Non-default entries that failed to build and were dropped with a
    /// warning (mirrors boot's failure policy).
    pub dropped: Vec<String>,
}

/// Why a reload was rejected. The atomic contract holds for every
/// variant: on `Err`, nothing was swapped.
#[derive(Debug, thiserror::Error)]
pub enum ReloadError {
    #[error("config load or validation failed: {0}")]
    Config(String),
    #[error("{0}")]
    NotHotReloadable(String),
    #[error("LLM pool rebuild failed (default entry unbuildable): {0}")]
    LlmRebuild(String),
    #[error("gateway was started without a config file; no reload target")]
    NoConfigPath,
}

/// Re-read the config file and atomically apply the whitelisted
/// changes. Implemented in the bin crate; held by the gateway so admin
/// endpoints and the SIGHUP handler can trigger it.
#[async_trait]
pub trait ConfigReloader: Send + Sync {
    /// Re-read the on-disk config and apply it, **always** rebuilding the
    /// LLM pool. The rebuild is unconditional because a vault credential
    /// rotation is invisible in the config diff — gating on the diff would
    /// let a rotated key keep serving the old credential. The rebuild is
    /// local/cheap and the per-turn client rebind is prompt-cache-safe, so
    /// there's no point gating it. See `docs/config-hot-reload.md`.
    async fn reload(&self) -> Result<ReloadOutcome, ReloadError>;

    /// Pre-flight a candidate config: rebuild its LLM pool to confirm the
    /// default model is buildable, **without** swapping, persisting, or
    /// committing anything. Admin endpoints call this before writing the
    /// candidate to disk, so a structurally-broken edit (e.g. an
    /// unbuildable default) is rejected *before* it dirties the file —
    /// which also stops a later SIGHUP from re-reading and silently
    /// dropping it. Deliberately does **not** apply the hot/non-hot
    /// whitelist; that stays with `reload`, so a generic endpoint can
    /// still persist a non-hot field (restart-pending) after a clean
    /// dry-run.
    async fn dry_run(&self, candidate: &AuraConfig) -> Result<(), ReloadError>;
}

impl From<ReloadError> for crate::error::GatewayError {
    fn from(e: ReloadError) -> Self {
        // Every reload rejection is operator-caused (invalid config, a
        // non-hot field changed, or an unbuildable default model), so a
        // 400 carrying the message is the right signal.
        crate::error::GatewayError::BadRequest(e.to_string())
    }
}
