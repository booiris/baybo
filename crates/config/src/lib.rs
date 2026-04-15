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
pub mod security;
pub mod session;
pub mod skills;
pub mod tools;
pub mod trace;
mod validate;
pub mod workspace;

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
pub use crate::security::SecurityConfig;
pub use crate::session::SessionConfig;
pub use crate::skills::{RiskCheckConfig, SkillsConfig};
pub use crate::tools::{ToolsConfig, TrustLevelConfig};
pub use crate::trace::TraceConfig;
pub use crate::workspace::WorkspaceConfig;

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
    pub security: SecurityConfig,
    pub skills: SkillsConfig,
    pub tools: ToolsConfig,
    pub trace: TraceConfig,
    pub cost: CostConfig,
    pub workspace: WorkspaceConfig,
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

    /// Serialize the config as pretty JSON and write it to `path` atomically-ish
    /// (replace via a tmpfile + rename). Used by `aura config set/unset`.
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

        let new_config: AuraConfig =
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

        let new_config: AuraConfig =
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
