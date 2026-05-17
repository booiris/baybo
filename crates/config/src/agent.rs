use serde::{Deserialize, Serialize};

/// Top-level agent configuration: execution policy and context window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AgentConfig {
    /// Maximum LLM iterations per user message before the loop stops.
    pub max_iterations: usize,
    /// Context window configuration.
    pub context: ContextConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 300,
            context: ContextConfig::default(),
        }
    }
}

/// Context window budget and compression settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ContextConfig {
    /// Fraction of the active model's context window at which
    /// compression triggers. Must be in `(0.0, 1.0]`.
    pub compression_threshold: f64,
    /// Number of most recent messages to retain when truncating.
    pub keep_recent: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            compression_threshold: 0.65,
            keep_recent: 10,
        }
    }
}
