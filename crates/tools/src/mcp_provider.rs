use std::collections::HashMap;
use std::sync::Arc;

use rmcp::RoleClient;
use rmcp::service::{RunningService, ServiceExt};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::child_process::TokioChildProcess;
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::RwLock;

use crate::ToolManifest;
use crate::mcp::{McpError, McpServerConfig, McpTool, McpTransport};

/// Manages connections to multiple MCP servers and discovers their tools.
pub struct McpToolProvider {
    servers: RwLock<HashMap<String, McpServerHandle>>,
}

struct McpServerHandle {
    #[allow(dead_code)]
    config: McpServerConfig,
    service: Arc<RwLock<RunningService<RoleClient, ()>>>,
}

impl Default for McpToolProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl McpToolProvider {
    pub fn new() -> Self {
        Self {
            servers: RwLock::new(HashMap::new()),
        }
    }

    /// Connect to an MCP server and discover its tools.
    ///
    /// Returns a list of `McpTool` instances ready for registration in `ToolRegistry`.
    pub async fn connect(
        &self,
        config: McpServerConfig,
    ) -> std::result::Result<Vec<McpTool>, McpError> {
        let service = self.create_service(&config).await?;

        let tools_result = service
            .list_all_tools()
            .await
            .map_err(|e| McpError::Protocol(e.to_string()))?;

        let service = Arc::new(RwLock::new(service));

        let tools = tools_result
            .into_iter()
            .map(|t| {
                let qualified_name = format!("{}/{}", config.name, t.name);
                let description = t.description.as_deref().unwrap_or_default().to_string();
                let parameters_schema = serde_json::to_value(&*t.input_schema)
                    .unwrap_or(Value::Object(Default::default()));

                let manifest = ToolManifest {
                    name: qualified_name.clone(),
                    description,
                    trust_level: config.trust_level.clone(),
                    parameters_schema,
                    secret_requirements: config.secret_requirements.clone(),
                    capabilities: config.capabilities.clone(),
                };

                McpTool {
                    qualified_name,
                    server_tool_name: t.name.to_string(),
                    manifest,
                    service: Arc::clone(&service),
                }
            })
            .collect();

        self.servers
            .write()
            .await
            .insert(config.name.clone(), McpServerHandle { config, service });

        Ok(tools)
    }

    /// Disconnect from an MCP server and clean up resources.
    pub async fn disconnect(&self, server_name: &str) -> std::result::Result<(), McpError> {
        let handle = self
            .servers
            .write()
            .await
            .remove(server_name)
            .ok_or_else(|| McpError::NotConnected {
                server: server_name.to_string(),
            })?;

        let service = Arc::try_unwrap(handle.service).map_err(|_| {
            McpError::Protocol(format!(
                "cannot disconnect '{server_name}': tools still in use"
            ))
        })?;

        let mut service = service.into_inner();
        service
            .close()
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;

        Ok(())
    }

    /// List all currently connected server names.
    pub async fn connected_servers(&self) -> Vec<String> {
        self.servers.read().await.keys().cloned().collect()
    }

    async fn create_service(
        &self,
        config: &McpServerConfig,
    ) -> std::result::Result<RunningService<RoleClient, ()>, McpError> {
        match &config.transport {
            McpTransport::Stdio { command, args, env } => {
                let mut cmd = Command::new(command);
                cmd.args(args);
                for (k, v) in env {
                    cmd.env(k, v);
                }
                let transport =
                    TokioChildProcess::new(cmd).map_err(|e| McpError::Transport(e.to_string()))?;
                ().serve(transport)
                    .await
                    .map_err(|e| McpError::ConnectionFailed(e.to_string()))
            }
            McpTransport::Http { url, .. } => {
                let transport = StreamableHttpClientTransport::from_uri(url.as_str());
                ().serve(transport)
                    .await
                    .map_err(|e| McpError::ConnectionFailed(e.to_string()))
            }
        }
    }
}
