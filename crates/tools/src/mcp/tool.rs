use async_trait::async_trait;
use rmcp::RoleClient;
use rmcp::model::{CallToolRequestParams, RawContent, Tool as RmcpTool};
use rmcp::service::Peer;
use serde_json::Value;

use crate::approval::ResourceAccess;
use crate::{Tool, ToolContext, ToolError, ToolOutput};

/// Wrapper that exposes one rmcp-discovered tool to Aura's agent loop.
///
/// Names are namespaced as `<server>/<tool>` so that an MCP server's tool
/// list cannot collide with a builtin. The peer is `Arc`-cloned per tool;
/// `Peer<RoleClient>` is internally `Arc`-based, so the cost is a refcount
/// bump per tool, not per call.
pub struct McpTool {
    server_name: String,
    tool_name: String,
    namespaced_name: String,
    description: String,
    parameters_schema: Value,
    resource_access: Vec<ResourceAccess>,
    peer: Peer<RoleClient>,
}

impl McpTool {
    pub fn new(
        server_name: String,
        descriptor: RmcpTool,
        resource_access: Vec<ResourceAccess>,
        peer: Peer<RoleClient>,
    ) -> Self {
        let namespaced_name = format!("{server_name}/{}", descriptor.name);
        let parameters_schema = serde_json::Value::Object((*descriptor.input_schema).clone());
        let description = descriptor
            .description
            .as_ref()
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("MCP tool {} on server {}", descriptor.name, server_name));
        Self {
            server_name,
            tool_name: descriptor.name.to_string(),
            namespaced_name,
            description,
            parameters_schema,
            resource_access,
            peer,
        }
    }

    pub fn server(&self) -> &str {
        &self.server_name
    }

    pub fn upstream_name(&self) -> &str {
        &self.tool_name
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.namespaced_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        self.parameters_schema.clone()
    }

    fn accessed_resources(&self, _params: &Value) -> Vec<ResourceAccess> {
        self.resource_access.clone()
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let mut request = CallToolRequestParams::new(self.tool_name.clone());
        match params {
            Value::Object(map) => {
                request = request.with_arguments(map);
            }
            Value::Null => {}
            other => {
                return Err(ToolError::InvalidParams(format!(
                    "MCP tools require an object of arguments; got {other:?}"
                )));
            }
        }

        let result =
            self.peer.call_tool(request).await.map_err(|e| {
                ToolError::Mcp(format!("{}/{}: {e}", self.server_name, self.tool_name))
            })?;

        if result.is_error.unwrap_or(false) {
            return Ok(ToolOutput::Error(format_content(&result.content)));
        }

        Ok(ToolOutput::Text(format_content(&result.content)))
    }
}

fn format_content(parts: &[rmcp::model::Annotated<RawContent>]) -> String {
    let mut out = String::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        match &part.raw {
            RawContent::Text(text) => out.push_str(&text.text),
            RawContent::Image(_) => out.push_str("[image content elided]"),
            RawContent::Audio(_) => out.push_str("[audio content elided]"),
            RawContent::Resource(_) => out.push_str("[resource content elided]"),
            RawContent::ResourceLink(_) => out.push_str("[resource link elided]"),
        }
    }
    out
}

/// Synthesize a ToolManifest for an MCP-sourced tool given the server's
/// trust + capabilities. Used by the reconciler when registering with
/// `ToolRegistry::register_dynamic`.
pub(crate) fn build_manifest(
    namespaced_name: &str,
    description: &str,
    parameters_schema: Value,
    trust_level: aura_model::TrustLevel,
    capabilities: Vec<crate::ToolCapability>,
) -> crate::ToolManifest {
    crate::ToolManifest {
        name: namespaced_name.to_string(),
        description: description.to_string(),
        trust_level,
        parameters_schema,
        capabilities,
    }
}
