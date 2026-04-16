use serde::{Deserialize, Serialize};

/// Tool execution configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ToolsConfig {
    /// Default tool execution timeout in milliseconds.
    pub default_timeout_ms: u64,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            default_timeout_ms: 30_000,
        }
    }
}

/// Mirror of `aura_model::TrustLevel`. The consumer maps between them.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevelConfig {
    Trusted,
    Installed,
    Untrusted,
}
