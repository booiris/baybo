use serde::{Deserialize, Serialize};

/// Top-level agent configuration: execution policy and context window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AgentConfig {
    /// Maximum LLM iterations per user message before the loop stops.
    pub max_iterations: usize,
    /// Context window configuration.
    pub context: ContextConfig,
    /// Side-channel memory + skill extraction flow that runs after a
    /// complex user-chat job completes. See
    /// `docs/modules/self-improvement.md`.
    pub self_improvement: SelfImprovementConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 20,
            context: ContextConfig::default(),
            self_improvement: SelfImprovementConfig::default(),
        }
    }
}

/// Mirror of [`aura_agent::self_improvement::SelfImprovementConfig`]. Lives
/// here so `aura-config` can stay decoupled from `aura-agent`. The
/// bootstrap layer in `src/runtime.rs` translates one to the other.
///
/// Defaults match the spec in `docs/modules/self-improvement.md`:
/// enabled, `min_iterations = 8`, `daily_cap = 100`,
/// `max_concurrent = 8`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SelfImprovementConfig {
    pub enabled: bool,
    pub min_iterations: u32,
    pub daily_cap: u32,
    pub max_concurrent: usize,
}

impl Default for SelfImprovementConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_iterations: 8,
            daily_cap: 100,
            max_concurrent: 8,
        }
    }
}

/// Context window budget and compression settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ContextConfig {
    /// Maximum tokens allowed in the context window.
    pub max_tokens: usize,
    /// Fraction of `max_tokens` at which compression triggers. Must be in `(0.0, 1.0]`.
    pub compression_threshold: f64,
    /// Number of most recent messages to retain when truncating.
    pub keep_recent: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_tokens: 120_000,
            compression_threshold: 0.75,
            keep_recent: 100,
        }
    }
}
