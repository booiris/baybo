pub mod registry;
pub mod wasm;

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use aura_core::User;
use aura_sandbox::{NetworkPolicy, SandboxPolicy};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Tool trait — the unified interface for all tool implementations.
///
/// Built-in Rust tools implement this directly.
/// WASM tools are adapted through `WasmTool`.
/// High-risk tools are still exposed as `Tool` but routed to container execution at runtime.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> Value;

    fn required_secrets(&self) -> Vec<String> {
        vec![]
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> aura_core::Result<ToolOutput>;
}

/// Context injected into tool execution by the agent layer.
pub struct ToolContext {
    pub session_id: String,
    pub user: User,
    pub timeout: Duration,
    pub cancellation_token: tokio_util::sync::CancellationToken,
    pub secrets: HashMap<String, SecretValue>,
    pub sandbox_policy: SandboxPolicy,
    pub network_policy: NetworkPolicy,
}

/// Opaque secret value that redacts itself in Debug output.
pub struct SecretValue {
    value: String,
}

impl SecretValue {
    pub fn new(value: String) -> Self {
        Self { value }
    }

    pub fn expose(&self) -> &str {
        &self.value
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[REDACTED]")
    }
}

/// Output from a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolOutput {
    Text(String),
    Json(Value),
    Error(String),
    LargeText { content: String, truncated: bool },
}

/// Definition visible to the LLM for function calling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,
}

/// Tool manifest carrying governance and runtime metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolManifest {
    pub name: String,
    pub description: String,
    pub version: String,
    pub artifact_hash: String,
    pub source: aura_core::ArtifactSource,
    pub trust_level: aura_core::TrustLevel,
    pub parameters_schema: Value,
    pub required_secrets: Vec<String>,
    pub capabilities: Vec<ToolCapability>,
    pub preferred_runtime: ToolRuntimeProfile,
}

/// Preferred execution runtime for a tool (advisory, not final).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRuntimeProfile {
    Wasm,
    Container,
}

/// Hard capability declarations for a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCapability {
    ReadWorkspace,
    WriteWorkspace,
    Http(Vec<String>),
    SpawnProcess,
    BrowserAutomation,
}

pub use registry::ToolRegistry;
pub use wasm::WasmTool;
