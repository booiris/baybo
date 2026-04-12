use serde::{Deserialize, Serialize};

/// Top-level agent configuration: execution policy and context window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AgentConfig {
    /// Maximum LLM iterations per user message before the loop stops.
    pub max_iterations: usize,
    /// Default timeout for tool execution in milliseconds.
    pub default_tool_timeout_ms: u64,
    /// Context window configuration.
    pub context: ContextConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 20,
            default_tool_timeout_ms: 30_000,
            context: ContextConfig::default(),
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
