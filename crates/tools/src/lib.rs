pub mod approval;
pub mod builtin;
pub mod error;
pub mod registry;

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use aura_model::User;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use approval::{
    ApprovalDecision, ApprovalGate, ApprovalGateMap, ApprovalQueue, ApprovalRequest,
    ApprovedResource, AutoDenyGate, ChannelApprovalGate, HostPattern, ResourceAccess,
};
pub use error::ToolError;

pub type Result<T> = std::result::Result<T, ToolError>;

// ---------------------------------------------------------------------------
// Tool trait
// ---------------------------------------------------------------------------

/// Tool trait — the unified interface for all tool implementations.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> Value;

    /// Resources this call will touch, derived from the parameters.
    ///
    /// The approval gate consults these at runtime before execution.
    /// Tools with no side effects return an empty vec (the default).
    fn accessed_resources(&self, _params: &Value) -> Vec<ResourceAccess> {
        Vec::new()
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput>;
}

/// Context injected into tool execution by the agent layer.
pub struct ToolContext {
    pub session_id: String,
    pub user: User,
    pub timeout: Duration,
    pub cancellation_token: tokio_util::sync::CancellationToken,
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
    pub capabilities: Vec<ToolCapability>,
}

/// Coarse capability ceiling declared in a tool's manifest.
///
/// A manifest capability says "this tool may do X at most"; the concrete
/// resource touched per call is described by [`ResourceAccess`] produced by
/// [`Tool::accessed_resources`]. The approval gate routes on `ResourceAccess`,
/// not on this enum.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolCapability {
    /// Reads from the filesystem. Approval gate prompts per path.
    ReadFile,
    /// Writes to the filesystem. Approval gate prompts per path.
    WriteFile,
    /// Performs network requests. Approval gate prompts per host.
    Http,
    /// Spawns a subprocess. Approval gate prompts per full command string.
    ExecCommand,
}

/// Convenience constructor for paths in [`ResourceAccess`] / [`ApprovedResource`].
pub fn resource_path(p: impl Into<PathBuf>) -> PathBuf {
    p.into()
}

pub use registry::ToolRegistry;
