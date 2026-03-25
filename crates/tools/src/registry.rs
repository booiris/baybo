use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use tracing::{debug, info, warn};

use crate::{Tool, ToolContext, ToolDefinition, ToolManifest, ToolOutput, WasmTool};
use aura_sandbox::WasmRuntime;

/// Central registry for all available tools (built-in and WASM).
pub struct ToolRegistry {
    builtin: HashMap<String, Arc<dyn Tool>>,
    wasm_tools: HashMap<String, WasmTool>,
    wasm_runtime: Arc<WasmRuntime>,
}

impl ToolRegistry {
    pub fn new(wasm_runtime: Arc<WasmRuntime>) -> Self {
        Self {
            builtin: HashMap::new(),
            wasm_tools: HashMap::new(),
            wasm_runtime,
        }
    }

    /// Register a built-in Rust tool.
    pub fn register_builtin(&mut self, tool: impl Tool + 'static) {
        let name = tool.name().to_string();
        debug!(tool_name = %name, "registering built-in tool");
        self.builtin.insert(name, Arc::new(tool));
    }

    /// Register a WASM tool from a directory containing `manifest.json` and
    /// a `.wasm` artifact.
    ///
    /// The directory is expected to have:
    /// - `manifest.json` — a `ToolManifest` describing the tool
    /// - A `.wasm` file whose SHA-256 hash matches `manifest.artifact_hash`
    pub fn register_wasm(&mut self, tool_dir: &Path) -> aura_core::Result<()> {
        let manifest_path = tool_dir.join("manifest.json");
        let manifest_bytes = std::fs::read(&manifest_path).map_err(|e| {
            aura_core::AuraError::Internal(anyhow::anyhow!(
                "failed to read manifest at {}: {e}",
                manifest_path.display()
            ))
        })?;

        let manifest: ToolManifest = serde_json::from_slice(&manifest_bytes).map_err(|e| {
            aura_core::AuraError::Serialization(format!(
                "failed to parse manifest at {}: {e}",
                manifest_path.display()
            ))
        })?;

        // Find the .wasm file in the directory
        let wasm_path = find_wasm_file(tool_dir)?;
        let wasm_bytes = std::fs::read(&wasm_path).map_err(|e| {
            aura_core::AuraError::Internal(anyhow::anyhow!(
                "failed to read WASM file at {}: {e}",
                wasm_path.display()
            ))
        })?;

        // Compile the module
        let module = self
            .wasm_runtime
            .load_module_named(&manifest.name, &wasm_bytes)
            .map_err(|e| aura_core::AuraError::Internal(anyhow::anyhow!("{e}")))?;

        // Validate artifact hash matches manifest
        if module.artifact_hash != manifest.artifact_hash {
            return Err(aura_core::AuraError::Security(format!(
                "artifact hash mismatch for tool '{}': manifest declares '{}' but computed '{}'",
                manifest.name, manifest.artifact_hash, module.artifact_hash
            )));
        }

        info!(
            tool_name = %manifest.name,
            version = %manifest.version,
            hash = %module.artifact_hash,
            "registered WASM tool"
        );

        let name = manifest.name.clone();
        let wasm_tool = WasmTool::new(manifest, Arc::clone(&self.wasm_runtime), Arc::new(module));
        self.wasm_tools.insert(name, wasm_tool);

        Ok(())
    }

    /// Load all WASM tools from a directory. Each subdirectory is expected to
    /// contain a `manifest.json` and a `.wasm` artifact.
    pub fn load_wasm_tools_from_dir(&mut self, dir: &Path) -> aura_core::Result<()> {
        let entries = std::fs::read_dir(dir).map_err(|e| {
            aura_core::AuraError::Internal(anyhow::anyhow!(
                "failed to read tool directory {}: {e}",
                dir.display()
            ))
        })?;

        for entry in entries {
            let entry = entry.map_err(|e| {
                aura_core::AuraError::Internal(anyhow::anyhow!(
                    "failed to read directory entry: {e}"
                ))
            })?;

            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            // Only attempt to load if a manifest.json exists
            if !path.join("manifest.json").exists() {
                debug!(path = %path.display(), "skipping directory without manifest.json");
                continue;
            }

            match self.register_wasm(&path) {
                Ok(()) => {}
                Err(e) => {
                    warn!(
                        path = %path.display(),
                        error = %e,
                        "failed to load WASM tool, skipping"
                    );
                }
            }
        }

        Ok(())
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
    ) -> aura_core::Result<ToolOutput> {
        let tool = self
            .get(name)
            .ok_or_else(|| aura_core::AuraError::NotFound(format!("tool not found: {name}")))?;
        tool.execute(params, ctx).await
    }

    /// Look up the manifest for a WASM tool by name.
    pub fn get_manifest(&self, name: &str) -> Option<&ToolManifest> {
        self.wasm_tools.get(name).map(|wt| &wt.manifest)
    }

    /// Returns true if the tool is a built-in (non-WASM) tool.
    pub fn is_builtin(&self, name: &str) -> bool {
        self.builtin.contains_key(name)
    }

    /// Returns the WASM runtime reference.
    pub fn wasm_runtime(&self) -> &Arc<WasmRuntime> {
        &self.wasm_runtime
    }
}

/// Find the first `.wasm` file in a directory.
fn find_wasm_file(dir: &Path) -> aura_core::Result<std::path::PathBuf> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        aura_core::AuraError::Internal(anyhow::anyhow!(
            "failed to read directory {}: {e}",
            dir.display()
        ))
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| {
            aura_core::AuraError::Internal(anyhow::anyhow!("failed to read directory entry: {e}"))
        })?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("wasm") {
            return Ok(path);
        }
    }

    Err(aura_core::AuraError::NotFound(format!(
        "no .wasm file found in {}",
        dir.display()
    )))
}
