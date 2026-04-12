use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Tool execution configuration and MCP server declarations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ToolsConfig {
    /// MCP (Model Context Protocol) servers to start alongside Aura.
    pub mcp_servers: Vec<McpServerEntry>,
    /// Default tool execution timeout in milliseconds.
    pub default_timeout_ms: u64,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            mcp_servers: Vec::new(),
            default_timeout_ms: 30_000,
        }
    }
}

/// A single MCP server entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpServerEntry {
    /// Unique name identifying the server.
    pub name: String,
    pub transport: McpTransportConfig,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secret_requirements: Vec<SecretRequirementConfig>,
    pub trust_level: TrustLevelConfig,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<CapabilityConfig>,
}

/// Transport used to talk to an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpTransportConfig {
    Stdio {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        env: HashMap<String, String>,
    },
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        headers: HashMap<String, String>,
    },
}

/// Mirror of `aura_registry::TrustLevel`. The consumer maps between them.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevelConfig {
    Trusted,
    Installed,
    Untrusted,
}

/// Mirror of `aura_tools::SecretAccess`. The consumer maps between them.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SecretAccessConfig {
    #[default]
    ReadOnly,
    ReadWrite,
}

/// Mirror of `aura_tools::ToolCapability`. The consumer maps between them.
/// Serialized in snake_case to match the domain type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityConfig {
    ReadWorkspace,
    WriteWorkspace,
    Http(Vec<String>),
    SpawnProcess,
    BrowserAutomation,
}

/// Declaration that a tool requires access to a named secret.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SecretRequirementConfig {
    /// Secret key, e.g. `"OPENAI_API_KEY"`.
    pub key: String,
    /// Access mode.
    #[serde(default)]
    pub access: SecretAccessConfig,
    /// Whether the secret is mandatory for the tool to run.
    #[serde(default = "default_true")]
    pub required: bool,
}

fn default_true() -> bool {
    true
}
