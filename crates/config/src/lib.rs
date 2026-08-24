//! Baybo configuration crate.
//!
//! Loads, validates, and exposes typed configuration for the Baybo runtime.
//! The top-level [`BayboConfig`] is deserialized from JSON and passed to the
//! consumer (usually `main.rs` or `baybo-agent`), which maps each section into
//! the corresponding domain type (e.g., [`LlmConfig`] → `baybo_llm::LlmProviderConfig`).
//!
//! ```no_run
//! use baybo_config::BayboConfig;
//!
//! # async fn demo() -> Result<(), baybo_config::ConfigError> {
//! let config = BayboConfig::load_from_file(std::path::Path::new("baybo.json")).await?;
//! # Ok(()) }
//! ```

pub mod agent;
pub mod browser;
pub mod channels;
pub mod cost;
pub mod error;
pub mod external_agents;
pub mod gateway;
pub mod llm;
pub mod memory;
pub mod permission;
pub mod proxy;
pub mod reload;
pub mod security;
pub mod skills;
pub mod tools;
mod validate;
pub mod web_search;
pub mod workspace;

use std::path::Path;

use serde::{Deserialize, Serialize};

pub use crate::agent::{AgentConfig, ContextConfig};
pub use crate::browser::{BrowserConfig, BrowserDockerConfig};
pub use crate::channels::{
    ChannelsConfig, CliChannelConfig, DiscordChannelConfig, TelegramChannelConfig,
};
pub use crate::cost::{CostConfig, RateLimitConfig, SpendingLimitsConfig};
pub use crate::error::{ConfigError, Result, ValidationError};
pub use crate::external_agents::{ClaudeConfig, CodexConfig, ExternalAgentsConfig};
pub use crate::gateway::GatewayConfig;
pub use crate::llm::{LlmEntry, LlmModelSpec, LlmPricingOverride};
pub use crate::memory::{MemoryConfig, MemoryProvider};
pub use crate::permission::PermissionPolicy;
pub use crate::proxy::ProxyConfig;
pub use crate::reload::{ConfigHandle, hot_reload_diff};
pub use crate::security::SecurityConfig;
pub use crate::skills::{RiskCheckConfig, SkillsConfig};
pub use crate::tools::TrustLevelConfig;
pub use crate::web_search::{WebSearchConfig, WebSearchProvider};
pub use crate::workspace::WorkspaceConfig;
pub use baybo_model::LlmEntryName;

/// Root configuration object for Baybo.
///
/// All fields have defaults, so deserializing an empty JSON object (`{}`)
/// yields a fully valid config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct BayboConfig {
    /// List of registered LLM entries. Multiple entries can target the
    /// same provider; each entry is keyed by its unique `name`.
    pub llm: Vec<LlmEntry>,
    /// Name of the entry in `llm` that the agent loop uses by default.
    /// Must reference an existing `name`.
    #[serde(rename = "default-llm")]
    pub default_llm: LlmEntryName,
    pub agent: AgentConfig,
    pub channels: ChannelsConfig,
    pub security: SecurityConfig,
    pub skills: SkillsConfig,
    pub cost: CostConfig,
    pub workspace: WorkspaceConfig,
    pub gateway: GatewayConfig,
    pub browser: BrowserConfig,
    pub external_agents: ExternalAgentsConfig,
    /// Optional egress proxy. When set, every outbound HTTP call (and every
    /// spawned child that makes its own HTTP) routes through it. Absent →
    /// direct connections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxyConfig>,
    pub memory: MemoryConfig,
    /// Pluggable web-search provider backing the `WebSearch` tool.
    pub web_search: WebSearchConfig,
    /// Permission policy for shell-out tools. Hot-reloadable.
    #[serde(default)]
    pub permission: PermissionPolicy,
}

impl BayboConfig {
    /// Look up an LLM entry by name. Returns the first match or `None`.
    pub fn llm_entry(&self, name: &str) -> Option<&LlmEntry> {
        self.llm.iter().find(|e| e.name == name)
    }

    /// Convenience: resolve the `default-llm` entry. Validation
    /// guarantees this is `Some` for any config that has been through
    /// `validate()`.
    pub fn default_llm_entry(&self) -> Option<&LlmEntry> {
        self.llm_entry(self.default_llm.as_str())
    }
}

impl BayboConfig {
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
        let config: BayboConfig =
            serde_json::from_str(json).map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// Serialize the config as pretty JSON and write it to `path` atomically-ish
    /// (replace via a tmpfile + rename). Used by `baybo config set/unset`.
    pub async fn write_to_file(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self).map_err(|e| ConfigError::FileWrite {
            path: path.display().to_string(),
            reason: format!("serialize: {e}"),
        })?;
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, json.as_bytes())
            .await
            .map_err(|e| ConfigError::FileWrite {
                path: tmp.display().to_string(),
                reason: e.to_string(),
            })?;
        tokio::fs::rename(&tmp, path)
            .await
            .map_err(|e| ConfigError::FileWrite {
                path: path.display().to_string(),
                reason: format!("rename from tmp: {e}"),
            })?;
        Ok(())
    }

    /// Set the value at `path` and return the resulting config (validated).
    ///
    /// `path` accepts both dotted (`llm.model`) and RFC 6901 slash
    /// (`/llm/model`) forms. `value` is any JSON value — strings, numbers,
    /// bools, objects, arrays.
    pub fn set_at_path(&self, path: &str, value: serde_json::Value) -> Result<Self> {
        let pointer = dotted_to_pointer(path);
        let mut root = serde_json::to_value(self).map_err(|e| ConfigError::Parse(e.to_string()))?;

        if pointer.is_empty() {
            return Err(ConfigError::InvalidPath {
                path: path.into(),
                reason: "path is empty (use e.g. 'llm.model')".into(),
            });
        }

        // Ensure parent objects exist; bail if any intermediate segment points at
        // a non-object (e.g. setting `llm.model.foo` when `llm.model` is a string).
        ensure_parent_path(&mut root, &pointer).map_err(|reason| ConfigError::InvalidPath {
            path: path.into(),
            reason,
        })?;

        let slot = root
            .pointer_mut(&pointer)
            .ok_or_else(|| ConfigError::InvalidPath {
                path: path.into(),
                reason: "could not resolve path".into(),
            })?;
        *slot = value;

        let new_config: BayboConfig =
            serde_json::from_value(root).map_err(|e| ConfigError::Parse(e.to_string()))?;
        new_config.validate()?;
        Ok(new_config)
    }

    /// Remove the field at `path` and return the resulting config (validated).
    /// Removing a field resets it to its serde default.
    pub fn unset_at_path(&self, path: &str) -> Result<Self> {
        let pointer = dotted_to_pointer(path);
        if pointer.is_empty() {
            return Err(ConfigError::InvalidPath {
                path: path.into(),
                reason: "cannot unset the whole config".into(),
            });
        }

        let mut root = serde_json::to_value(self).map_err(|e| ConfigError::Parse(e.to_string()))?;

        let (parent_ptr, leaf) = split_pointer(&pointer);
        let parent = root
            .pointer_mut(parent_ptr)
            .ok_or_else(|| ConfigError::InvalidPath {
                path: path.into(),
                reason: "parent path does not exist".into(),
            })?;
        let obj = parent
            .as_object_mut()
            .ok_or_else(|| ConfigError::InvalidPath {
                path: path.into(),
                reason: "parent is not a JSON object".into(),
            })?;
        obj.remove(leaf);

        let new_config: BayboConfig =
            serde_json::from_value(root).map_err(|e| ConfigError::Parse(e.to_string()))?;
        new_config.validate()?;
        Ok(new_config)
    }
}

fn dotted_to_pointer(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else if path.is_empty() {
        String::new()
    } else {
        let mut out = String::with_capacity(path.len() + 1);
        for seg in path.split('.') {
            if seg.is_empty() {
                continue;
            }
            out.push('/');
            // RFC 6901 escape: ~ → ~0, / → ~1
            for ch in seg.chars() {
                match ch {
                    '~' => out.push_str("~0"),
                    '/' => out.push_str("~1"),
                    c => out.push(c),
                }
            }
        }
        out
    }
}

fn split_pointer(pointer: &str) -> (&str, &str) {
    match pointer.rfind('/') {
        Some(idx) => (&pointer[..idx], &pointer[idx + 1..]),
        None => ("", pointer),
    }
}

fn ensure_parent_path(
    root: &mut serde_json::Value,
    pointer: &str,
) -> std::result::Result<(), String> {
    // Walk every prefix ending in '/' — create missing object children.
    let (parent, _) = split_pointer(pointer);
    if parent.is_empty() {
        return Ok(());
    }
    let mut cursor: &mut serde_json::Value = root;
    for seg in parent.trim_start_matches('/').split('/') {
        let key = seg.replace("~1", "/").replace("~0", "~");
        // Borrow-split: move cursor step-by-step through the tree, creating
        // missing intermediate objects as we go.
        let obj = cursor
            .as_object_mut()
            .ok_or_else(|| format!("segment '{key}' is not under an object"))?;
        cursor = obj
            .entry(key.clone())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
        if !cursor.is_object() {
            return Err(format!(
                "segment '{key}' exists but is not an object; cannot descend"
            ));
        }
    }
    Ok(())
}
