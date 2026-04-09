use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::{Tool, ToolContext, ToolDefinition, ToolManifest, ToolOutput, WasmTool};

/// Central registry for all available tools (built-in and WASM).
pub struct ToolRegistry {
    builtin: HashMap<String, Arc<dyn Tool>>,
    wasm_tools: HashMap<String, WasmTool>,
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
        }
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

    /// Look up the manifest for a WASM tool by name.
    pub fn get_manifest(&self, name: &str) -> Option<&ToolManifest> {
        self.wasm_tools.get(name).map(|wt| &wt.manifest)
    }
}
