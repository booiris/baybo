use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use baybo_store::BlobStore;
use rmcp::RoleClient;
use rmcp::model::{CallToolRequestParams, Tool as RmcpTool};
use rmcp::service::Peer;
use serde_json::Value;

use crate::approval::ResourceAccess;
use crate::mcp::content_adapter::adapt_call_result;
use crate::{Tool, ToolContext, ToolError, ToolOutput};

/// Wrapper that exposes one rmcp-discovered tool to Baybo's agent loop.
///
/// Names are namespaced as `<server>/<tool>` so an MCP server's tool list
/// cannot collide with a builtin. The peer is `Arc`-cloned per tool;
/// `Peer<RoleClient>` is internally `Arc`-based, so the cost is a refcount
/// bump per tool, not per call.
///
/// `default_resource_access` is the per-server resource list the approval
/// gate consults — every tool from the same server inherits the same
/// access shape (stdio → one `ExecCommand`, http → one `Http`, embedded
/// servers with `capabilities=[]` → none). MCP tools have no per-call
/// override path: a future embedded server that needs finer per-tool
/// approvals adds a typed mechanism for that case at the time it ships.
pub struct McpTool {
    server_name: String,
    tool_name: String,
    namespaced_name: String,
    description: String,
    parameters_schema: Value,
    default_resource_access: Vec<ResourceAccess>,
    peer: Peer<RoleClient>,
    blob_store: Option<Arc<dyn BlobStore>>,
}

impl McpTool {
    pub fn new(
        server_name: String,
        descriptor: RmcpTool,
        default_resource_access: Vec<ResourceAccess>,
        peer: Peer<RoleClient>,
        blob_store: Option<Arc<dyn BlobStore>>,
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
            default_resource_access,
            peer,
            blob_store,
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

    fn description(&self) -> String {
        self.description.clone()
    }

    fn parameters_schema(&self) -> Value {
        self.parameters_schema.clone()
    }

    fn accessed_resources(&self, _params: &Value) -> Vec<ResourceAccess> {
        self.default_resource_access.clone()
    }

    fn max_timeout(&self) -> Duration {
        // Upstream MCP servers can be anything from a quick stdio
        // round-trip to a remote HTTP API doing real work. The
        // trait-default 30 s is too tight for the latter; 60 s is a
        // reasonable common ceiling. A future per-server config
        // (`McpServerEntry::max_timeout_secs`) can override this
        // when an operator knows their server needs more headroom.
        Duration::from_secs(60)
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

        let is_error = result.is_error.unwrap_or(false);
        adapt_call_result(&result.content, is_error, self.blob_store.as_ref()).await
    }
}

/// Synthesize a ToolManifest for an MCP-sourced tool given the server's
/// trust + capabilities. Used by the reconciler when registering with
/// `ToolRegistry::register_dynamic`.
pub(crate) fn build_manifest(
    namespaced_name: &str,
    description: String,
    parameters_schema: Value,
    trust_level: baybo_model::TrustLevel,
    capabilities: Vec<crate::ToolCapability>,
) -> crate::ToolManifest {
    crate::ToolManifest {
        name: namespaced_name.to_string(),
        description,
        trust_level,
        parameters_schema,
        capabilities,
        channels: Vec::new(),
    }
}
