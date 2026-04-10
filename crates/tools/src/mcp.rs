use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use rmcp::RoleClient;
use rmcp::model::{CallToolRequestParams, Content, RawContent};
use rmcp::service::RunningService;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;

use crate::{SecretRequirement, Tool, ToolCapability, ToolContext, ToolManifest, ToolOutput};
use aura_registry::TrustLevel;

/// Errors specific to MCP operations.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("MCP transport error: {0}")]
    Transport(String),

    #[error("MCP protocol error: {0}")]
    Protocol(String),

    #[error("MCP server '{server}' not connected")]
    NotConnected { server: String },

    #[error("MCP connection failed: {0}")]
    ConnectionFailed(String),
}

/// Transport type for connecting to an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpTransport {
    /// Connect via stdio (spawn a subprocess).
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    /// Connect via streamable HTTP.
    Http {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

/// Configuration for connecting to a single MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Human-readable name for this MCP server (used as namespace prefix).
    pub name: String,
    /// Transport mechanism.
    pub transport: McpTransport,
    /// Secret requirements for tools from this server.
    #[serde(default)]
    pub secret_requirements: Vec<SecretRequirement>,
    /// Trust level assigned to tools from this server.
    pub trust_level: TrustLevel,
    /// Capabilities granted to tools from this server.
    #[serde(default)]
    pub capabilities: Vec<ToolCapability>,
}

/// An MCP tool adapter that implements the `Tool` trait.
///
/// Each instance represents a single tool discovered from an MCP server.
/// The tool name is namespaced as `{server_name}/{tool_name}`.
pub struct McpTool {
    pub(crate) qualified_name: String,
    pub(crate) server_tool_name: String,
    pub(crate) manifest: ToolManifest,
    pub(crate) service: Arc<RwLock<RunningService<RoleClient, ()>>>,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.qualified_name
    }

    fn description(&self) -> &str {
        &self.manifest.description
    }

    fn parameters_schema(&self) -> Value {
        self.manifest.parameters_schema.clone()
    }

    fn secret_requirements(&self) -> Vec<SecretRequirement> {
        self.manifest.secret_requirements.clone()
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let arguments = params.as_object().cloned();

        let mut call = CallToolRequestParams::new(self.server_tool_name.clone());
        if let Some(args) = arguments {
            call = call.with_arguments(args);
        }

        let service = self.service.read().await;
        let result = service
            .call_tool(call)
            .await
            .map_err(|e| crate::ToolError::Mcp(e.to_string()))?;

        if result.is_error == Some(true) {
            let error_text = extract_text_content(&result.content);
            return Ok(ToolOutput::Error(error_text));
        }

        let text = extract_text_content(&result.content);
        if let Ok(json) = serde_json::from_str::<Value>(&text) {
            Ok(ToolOutput::Json(json))
        } else {
            Ok(ToolOutput::Text(text))
        }
    }
}

/// Extract text from MCP content blocks.
fn extract_text_content(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|c| match &c.raw {
            RawContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
