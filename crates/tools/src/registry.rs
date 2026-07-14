use std::collections::HashMap;
use std::sync::Arc;

use baybo_store::BlobStore;
use baybo_workspace::WorkspacePaths;
use parking_lot::RwLock;
use serde_json::Value;

use crate::{Tool, ToolConcurrency, ToolContext, ToolDefinition, ToolManifest, ToolOutput};

pub struct ToolRegistry {
    builtin: HashMap<String, Arc<dyn Tool>>,
    builtin_manifests: HashMap<String, ToolManifest>,
    dynamic: RwLock<DynamicState>,
}

#[derive(Default)]
struct DynamicState {
    tools: HashMap<String, Arc<dyn Tool>>,
    manifests: HashMap<String, ToolManifest>,
    by_source: HashMap<String, Vec<String>>,
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
            builtin_manifests: HashMap::new(),
            dynamic: RwLock::new(DynamicState::default()),
        }
    }

    /// Register every default builtin tool. `WebFetch`'s prompt-driven
    /// extraction reads its LLM handle from per-call
    /// [`crate::ToolContext::llm`], so no LLM client is threaded here;
    /// the agent layer binds the handle when it materialises each
    /// tool-call's context. `workspace_paths` is forwarded to `Edit`
    /// so its approval-gate bypass for `profile/` writes can bind to
    /// the real workspace rather than a heuristic on the path string.
    /// `permission` is the shared, hot-swappable handle that drives
    /// `BashTool`'s isolation/approval behavior and the description it
    /// advertises; a config reload swaps it live.
    pub fn with_defaults(
        blob_store: Arc<dyn BlobStore>,
        workspace_paths: WorkspacePaths,
        proxy: Option<reqwest::Proxy>,
        permission: Arc<crate::builtin::LivePermissionMode>,
    ) -> Self {
        let mut registry = Self::new();
        for (tool, manifest) in
            crate::builtin::default_tools(blob_store, workspace_paths, proxy, permission)
        {
            registry.register(tool, manifest);
        }
        registry
    }

    /// Register a builtin tool together with its governance manifest.
    ///
    /// Builtin registration is single-writer at startup; only the last
    /// registration for a given name wins.
    pub fn register(&mut self, tool: Arc<dyn Tool>, manifest: ToolManifest) {
        let name = tool.name().to_string();
        debug_assert_eq!(
            name, manifest.name,
            "tool name does not match manifest name"
        );
        self.builtin_manifests.insert(name.clone(), manifest);
        self.builtin.insert(name, tool);
    }

    /// Register a tool sourced from an external provider (an MCP server,
    /// for example). `source` is the logical owner the reconciler uses
    /// for [`Self::unregister_for_source`]; tools should be named with a
    /// `<source>/<tool>` convention to avoid colliding with builtins.
    pub fn register_dynamic(&self, source: &str, tool: Arc<dyn Tool>, manifest: ToolManifest) {
        let name = tool.name().to_string();
        debug_assert_eq!(
            name, manifest.name,
            "tool name does not match manifest name"
        );

        if self.builtin.contains_key(&name) {
            tracing::warn!(
                tool = %name,
                source = %source,
                "dynamic tool shadows a builtin; the dynamic registration wins"
            );
        }

        let mut state = self.dynamic.write();
        state.tools.insert(name.clone(), tool);
        state.manifests.insert(name.clone(), manifest);
        state
            .by_source
            .entry(source.to_string())
            .or_default()
            .push(name);
    }

    /// Drop every dynamic tool previously registered under `source`.
    /// Returns the names that were removed.
    pub fn unregister_for_source(&self, source: &str) -> Vec<String> {
        let mut state = self.dynamic.write();
        let names = state.by_source.remove(source).unwrap_or_default();
        for name in &names {
            state.tools.remove(name);
            state.manifests.remove(name);
        }
        names
    }

    /// List the names of every dynamic tool currently registered under
    /// `source`. The reconciler consults this to diff what is connected
    /// against what the config asks for.
    pub fn dynamic_names_for_source(&self, source: &str) -> Vec<String> {
        self.dynamic
            .read()
            .by_source
            .get(source)
            .cloned()
            .unwrap_or_default()
    }

    /// Generate tool definitions visible to the LLM. Dynamic tools win on
    /// name collision so a reconciler-installed tool can shadow a builtin
    /// of the same name (the warning logged in `register_dynamic` is the
    /// audit trail).
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        let mut defs: HashMap<String, ToolDefinition> = HashMap::new();
        for tool in self.builtin.values() {
            let def = tool_definition_for(tool.as_ref());
            defs.insert(def.name.clone(), def);
        }
        for tool in self.dynamic.read().tools.values() {
            let def = tool_definition_for(tool.as_ref());
            defs.insert(def.name.clone(), def);
        }
        // Sort by name so the serialized `tools` array is byte-identical
        // across calls. Anthropic's prompt cache keys on the exact
        // request prefix (tools → system → messages); a HashMap-driven
        // shuffle invalidates every breakpoint past `tools`, and the
        // cache miss shows up as `cached_input_tokens` ≪ `input_tokens`
        // on multi-turn runs.
        let mut out: Vec<ToolDefinition> = defs.into_values().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Look up a tool by name. Dynamic registrations shadow builtins.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if let Some(t) = self.dynamic.read().tools.get(name) {
            return Some(Arc::clone(t));
        }
        self.builtin.get(name).cloned()
    }

    /// Short progress preview for a pending call via
    /// [`Tool::progress_label`] (Bash's command, WebFetch's URL, …).
    /// `None` when the tool is unregistered or declares no preview. Used by
    /// the agent loop to caption `ToolStarted` progress events.
    pub fn progress_label(&self, name: &str, params: &Value) -> Option<String> {
        self.get(name).and_then(|tool| tool.progress_label(params))
    }

    /// Concurrency policy for a pending call via [`Tool::concurrency`].
    /// Unknown tools default to [`ToolConcurrency::Exclusive`] (fail
    /// safe — never parallelize a call we cannot classify). The agent
    /// loop uses this to size how many permits the call holds before it
    /// runs.
    pub fn concurrency(&self, name: &str) -> ToolConcurrency {
        self.get(name)
            .map(|tool| tool.concurrency())
            .unwrap_or(ToolConcurrency::Exclusive)
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

    /// Look up the manifest for a registered tool by name. Dynamic
    /// manifests shadow builtins to match [`Self::get`].
    pub fn get_manifest(&self, name: &str) -> Option<ToolManifest> {
        if let Some(m) = self.dynamic.read().manifests.get(name) {
            return Some(m.clone());
        }
        self.builtin_manifests.get(name).cloned()
    }
}

fn tool_definition_for(tool: &dyn Tool) -> ToolDefinition {
    ToolDefinition {
        name: tool.name().to_string(),
        description: tool.description(),
        parameters_schema: tool.parameters_schema(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use baybo_storage::test_support::MemoryBlobStore;
    use baybo_store::BlobStore;

    use super::ToolRegistry;
    use crate::ToolConcurrency;

    fn default_registry() -> ToolRegistry {
        let blob_store = Arc::new(MemoryBlobStore::new()) as Arc<dyn BlobStore>;
        ToolRegistry::with_defaults(
            blob_store,
            baybo_workspace::WorkspacePaths::new("/tmp"),
            None,
            Arc::new(crate::builtin::LivePermissionMode::new(
                crate::builtin::BashPermissionMode::Manual,
            )),
        )
    }

    #[test]
    fn defaults_register_attach_file() {
        let registry = default_registry();

        assert!(registry.get("AttachFile").is_some());
        assert!(registry.get_manifest("AttachFile").is_some());
    }

    #[test]
    fn read_only_builtins_are_concurrent() {
        let registry = default_registry();

        // Read-only builtins opt into concurrent execution; the rest
        // (writers, exec, blob-staging) keep the exclusive default.
        for name in [
            "Read",
            "Glob",
            "Grep",
            "WebFetch",
            "Now",
            "SecretList",
            "SecretCheck",
        ] {
            assert_eq!(
                registry.concurrency(name),
                ToolConcurrency::Concurrent,
                "{name} should be concurrent"
            );
        }
        for name in ["Write", "Edit", "Bash", "AttachFile", "SecretAdd"] {
            assert_eq!(
                registry.concurrency(name),
                ToolConcurrency::Exclusive,
                "{name} must stay exclusive"
            );
        }
    }

    #[test]
    fn unknown_tool_fails_safe_to_exclusive() {
        let registry = default_registry();
        assert_eq!(
            registry.concurrency("NoSuchTool"),
            ToolConcurrency::Exclusive,
            "an unclassifiable tool must never be parallelized"
        );
    }
}
