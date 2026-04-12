//! Aura configuration crate.
//!
//! Loads, validates, and exposes typed configuration for the Aura runtime.
//! The top-level [`AuraConfig`] is deserialized from JSON and passed to the
//! consumer (usually `main.rs` or `aura-agent`), which maps each section into
//! the corresponding domain type (e.g., [`LlmConfig`] → `aura_llm::LlmProviderConfig`).
//!
//! ```no_run
//! use aura_config::AuraConfig;
//!
//! # async fn demo() -> Result<(), aura_config::ConfigError> {
//! let config = AuraConfig::load_from_file(std::path::Path::new("aura.json")).await?;
//! # Ok(()) }
//! ```

pub mod agent;
pub mod channels;
pub mod cost;
pub mod error;
pub mod llm;
pub mod sandbox;
pub mod security;
pub mod session;
pub mod tools;
pub mod trace;
mod validate;

use std::path::Path;

use serde::{Deserialize, Serialize};

pub use crate::agent::{AgentConfig, ContextConfig};
pub use crate::channels::{
    ChannelsConfig, CliChannelConfig, DiscordChannelConfig, HttpChannelConfig,
    TelegramChannelConfig,
};
pub use crate::cost::{CostConfig, RateLimitConfig, SpendingLimitsConfig};
pub use crate::error::{ConfigError, Result, ValidationError};
pub use crate::llm::LlmConfig;
pub use crate::sandbox::{NetworkPolicyConfig, SandboxConfig, WasmLimitsConfig};
pub use crate::security::SecurityConfig;
pub use crate::session::SessionConfig;
pub use crate::tools::{
    CapabilityConfig, McpServerEntry, McpTransportConfig, SecretAccessConfig,
    SecretRequirementConfig, ToolsConfig, TrustLevelConfig,
};
pub use crate::trace::TraceConfig;

/// Root configuration object for Aura.
///
/// All sections have defaults, so deserializing an empty JSON object (`{}`)
/// yields a fully valid config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct AuraConfig {
    pub llm: LlmConfig,
    pub agent: AgentConfig,
    pub session: SessionConfig,
    pub channels: ChannelsConfig,
    pub sandbox: SandboxConfig,
    pub security: SecurityConfig,
    pub tools: ToolsConfig,
    pub trace: TraceConfig,
    pub cost: CostConfig,
}

impl AuraConfig {
    /// Read, parse, and validate a config file.
    pub async fn load_from_file(path: &Path) -> Result<Self> {
        let contents =
            tokio::fs::read_to_string(path)
                .await
                .map_err(|e| ConfigError::FileRead {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                })?;
        Self::load_from_str(&contents)
    }

    /// Parse and validate a config from a JSON string.
    pub fn load_from_str(json: &str) -> Result<Self> {
        let config: AuraConfig =
            serde_json::from_str(json).map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }
}
