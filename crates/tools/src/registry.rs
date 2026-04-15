use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::{Tool, ToolContext, ToolDefinition, ToolManifest, ToolOutput};

/// Central registry for all available tools.
pub struct ToolRegistry {
    builtin: HashMap<String, Arc<dyn Tool>>,
    manifests: HashMap<String, ToolManifest>,
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
            manifests: HashMap::new(),
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
        defs
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.builtin.get(name).map(|t| t.as_ref())
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
        self.manifests.get(name)
    }
}
