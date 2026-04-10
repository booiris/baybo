use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::mcp::McpTool;
use crate::{Tool, ToolContext, ToolDefinition, ToolManifest, ToolOutput, WasmTool};

/// Central registry for all available tools (built-in, WASM, and MCP).
pub struct ToolRegistry {
    builtin: HashMap<String, Arc<dyn Tool>>,
    wasm_tools: HashMap<String, WasmTool>,
    mcp_tools: HashMap<String, McpTool>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            builtin: HashMap::new(),
            wasm_tools: HashMap::new(),
            mcp_tools: HashMap::new(),
        }
    }

    /// Register an MCP tool discovered from an MCP server.
    pub fn register_mcp_tool(&mut self, tool: McpTool) {
        self.mcp_tools.insert(tool.qualified_name.clone(), tool);
    }

    /// Remove all MCP tools belonging to a given server.
    pub fn remove_mcp_tools_for_server(&mut self, server_name: &str) {
        let prefix = format!("{server_name}/");
        self.mcp_tools.retain(|name, _| !name.starts_with(&prefix));
    }

    /// Generate tool definitions visible to the LLM.
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let mut defs = Vec::new();
        for tool in self.builtin.values() {
            defs.push(ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters_schema: tool.parameters_schema(),
            });
        }
        for wasm_tool in self.wasm_tools.values() {
            defs.push(ToolDefinition {
                name: wasm_tool.manifest.name.clone(),
                description: wasm_tool.manifest.description.clone(),
                parameters_schema: wasm_tool.manifest.parameters_schema.clone(),
            });
        }
        for mcp_tool in self.mcp_tools.values() {
            defs.push(ToolDefinition {
                name: mcp_tool.name().to_string(),
                description: mcp_tool.description().to_string(),
                parameters_schema: mcp_tool.parameters_schema(),
            });
        }
        defs
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        if let Some(tool) = self.builtin.get(name) {
            return Some(tool.as_ref());
        }
        if let Some(wasm_tool) = self.wasm_tools.get(name) {
            return Some(wasm_tool as &dyn Tool);
        }
        if let Some(mcp_tool) = self.mcp_tools.get(name) {
            return Some(mcp_tool as &dyn Tool);
        }
        None
    }

    /// Execute a tool by name with the given parameters and context.
    pub async fn execute(
        &self,
        name: &str,
        params: Value,
        ctx: &ToolContext,
    ) -> crate::Result<ToolOutput> {
        let tool = self
            .get(name)
            .ok_or_else(|| crate::ToolError::NotFound(format!("tool not found: {name}")))?;
        tool.execute(params, ctx).await
    }

    /// Look up the manifest for a non-builtin tool by name.
    pub fn get_manifest(&self, name: &str) -> Option<&ToolManifest> {
        if let Some(wt) = self.wasm_tools.get(name) {
            return Some(&wt.manifest);
        }
        if let Some(mt) = self.mcp_tools.get(name) {
            return Some(&mt.manifest);
        }
        None
    }
}
