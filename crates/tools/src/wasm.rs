use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use aura_sandbox::{WasmModule, WasmRuntime};

use crate::{Tool, ToolContext, ToolManifest, ToolOutput};

/// A WASM tool loaded from a manifest and compiled module.
pub struct WasmTool {
    pub manifest: ToolManifest,
    runtime: Arc<WasmRuntime>,
    module: Arc<WasmModule>,
}

impl WasmTool {
    pub fn new(manifest: ToolManifest, runtime: Arc<WasmRuntime>, module: Arc<WasmModule>) -> Self {
        Self {
            manifest,
            runtime,
            module,
        }
    }
}

#[async_trait]
impl Tool for WasmTool {
    fn name(&self) -> &str {
        &self.manifest.name
    }

    fn description(&self) -> &str {
        &self.manifest.description
    }

    fn parameters_schema(&self) -> Value {
        self.manifest.parameters_schema.clone()
    }

    fn required_secrets(&self) -> Vec<String> {
        self.manifest.required_secrets.clone()
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> aura_core::Result<ToolOutput> {
        // Collect secrets into a plain HashMap<String, String> for the WASM ABI
        let mut secrets = HashMap::new();
        for (k, v) in &ctx.secrets {
            secrets.insert(k.clone(), v.expose().to_string());
        }

        let result = self
            .runtime
            .execute(&self.module, params, secrets)
            .await
            .map_err(|e| aura_core::AuraError::Internal(anyhow::anyhow!("{e}")))?;

        match result {
            Value::Null => Ok(ToolOutput::Text(String::new())),
            Value::String(s) => Ok(ToolOutput::Text(s)),
            other => Ok(ToolOutput::Json(other)),
        }
    }
}
