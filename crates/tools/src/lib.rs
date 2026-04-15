pub mod error;
pub mod registry;
pub mod wasm;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aura_model::User;
use aura_sandbox::{NetworkPolicy, SandboxPolicy};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use error::ToolError;

pub type Result<T> = std::result::Result<T, ToolError>;

// ---------------------------------------------------------------------------
// Secret declaration types
// ---------------------------------------------------------------------------

/// Access level for a secret key declared by a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretAccess {
    /// Tool may read the secret value but not modify it.
    ReadOnly,
    /// Tool may both read and write (create/update) the secret value.
    ReadWrite,
}

/// A single secret requirement declared by a tool at registration time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretRequirement {
    /// The secret key name (e.g. "GITHUB_TOKEN").
    pub key: String,
    /// Whether the tool needs read-only or read-write access.
    pub access: SecretAccess,
    /// Whether execution should fail if this secret is missing from the vault.
    pub required: bool,
}

impl SecretRequirement {
    pub fn read_only(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            access: SecretAccess::ReadOnly,
            required: true,
        }
    }

    pub fn read_write(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            access: SecretAccess::ReadWrite,
            required: true,
        }
    }

    pub fn optional(mut self) -> Self {
        self.required = false;
        self
    }
}

// ---------------------------------------------------------------------------
// Secret accessor
// ---------------------------------------------------------------------------

/// Errors from secret access operations.
#[derive(Debug, thiserror::Error)]
pub enum SecretAccessError {
    #[error("secret key '{key}' was not declared by this tool")]
    Undeclared { key: String },

    #[error("secret key '{key}' is declared as read-only")]
    ReadOnlyViolation { key: String },

    #[error("secret not found: {key}")]
    NotFound { key: String },

    #[error("secret access error: {0}")]
    Internal(String),
}

/// Runtime accessor for secrets during tool execution.
///
/// Provides scoped, permission-checked access to secrets.
/// Implementations enforce that a tool can only access keys it declared,
/// and can only write keys declared with `ReadWrite` access.
#[async_trait]
pub trait SecretAccessor: Send + Sync {
    /// Retrieve a secret value by key.
    async fn get(&self, key: &str) -> std::result::Result<SecretValue, SecretAccessError>;

    /// Store or update a secret value. Requires `ReadWrite` access on the key.
    async fn set(&self, key: &str, value: &[u8]) -> std::result::Result<(), SecretAccessError>;
}

// ---------------------------------------------------------------------------
// Tool trait
// ---------------------------------------------------------------------------

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

    /// Declare which secrets this tool needs and at what access level.
    fn secret_requirements(&self) -> Vec<SecretRequirement> {
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
    /// Static snapshot of secrets injected at execution start.
    /// Kept for WASM tools that read secrets via host functions.
    pub secrets: HashMap<String, SecretValue>,
    /// Runtime secret accessor with permission enforcement.
    /// `None` for WASM tools (they use the static `secrets` map).
    pub secret_accessor: Option<Arc<dyn SecretAccessor>>,
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
    pub secret_requirements: Vec<SecretRequirement>,
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
