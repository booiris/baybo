pub mod error;
pub mod registry;
pub mod wasm;

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use aura_sandbox::{NetworkPolicy, SandboxPolicy};
use aura_session::User;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use error::ToolError;

pub type Result<T> = std::result::Result<T, ToolError>;

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

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput>;
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
    pub trust_level: aura_registry::TrustLevel,
    pub parameters_schema: Value,
    pub required_secrets: Vec<String>,
    pub capabilities: Vec<ToolCapability>,
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
