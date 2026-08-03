use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use serde_json::Value;

use crate::{
    LoadedTools, Tool, ToolConcurrency, ToolContext, ToolDefinition, ToolManifest, ToolOutput,
};

/// The three axes that decide whether one session may see and call a tool.
///
/// Grouped rather than passed as three references because the visibility
/// filter here and the executor's admission check have to apply the *same*
/// three: a tool omitted from the list but accepted on the way in is not gated
/// at all, and a hallucinated or skill-body-prompted name would still run.
#[derive(Clone, Copy)]
pub struct SessionToolScope<'a> {
    pub channel: &'a baybo_model::ChannelType,
    pub trigger: &'a baybo_model::TriggerSource,
    pub loaded: &'a LoadedTools,
}

impl SessionToolScope<'_> {
    /// Whether this session may see and call `tool`. `manifest` is `None` only
    /// for a registration that somehow carries none, treated as unrestricted.
    pub fn allows(&self, manifest: Option<&ToolManifest>, tool: &dyn Tool) -> bool {
        let manifest_ok =
            manifest.is_none_or(|m| m.allows_channel(self.channel) && m.is_loaded(self.loaded));
        manifest_ok && tool.trigger_scope().allows_trigger(self.trigger)
    }

    /// Like [`Self::allows`] but ignoring whether the tool has been loaded —
    /// "would this session be allowed to load it at all". `ToolSearch` uses
    /// this so a channel- or trigger-restricted tool cannot be reached by
    /// naming it, and so the roster never advertises one.
    pub fn allows_ignoring_load(&self, manifest: Option<&ToolManifest>, tool: &dyn Tool) -> bool {
        manifest.is_none_or(|m| m.allows_channel(self.channel))
            && tool.trigger_scope().allows_trigger(self.trigger)
    }
}

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
    /// so its approval-gate bypass for `personas/` writes can bind to
    /// the real workspace rather than a heuristic on the path string.
    /// `permission` is the shared, hot-swappable handle that drives
    /// `BashTool`'s isolation/approval behavior and the description it
    /// advertises; a config reload swaps it live.
    pub fn with_defaults(config: crate::builtin::DefaultToolsConfig) -> Self {
        let mut registry = Self::new();
        for (tool, manifest) in crate::builtin::default_tools(config) {
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

    /// Register a builtin that could not be built before the registry was
    /// frozen — one that needs a handle to the registry itself.
    ///
    /// Deliberately not [`Self::register_dynamic`]: that seam belongs to
    /// external providers, and its `source` is what
    /// [`Self::unregister_for_source`] sweeps. Filing a first-party builtin
    /// under an invented source name would put it in reach of a sweep that has
    /// no business knowing about it, and list it beside real MCP servers.
    ///
    /// The entry therefore carries no source and cannot be unregistered.
    pub fn register_late_builtin(&self, tool: Arc<dyn Tool>, manifest: ToolManifest) {
        let name = tool.name().to_string();
        debug_assert_eq!(
            name, manifest.name,
            "tool name does not match manifest name"
        );
        let mut state = self.dynamic.write();
        state.tools.insert(name.clone(), tool);
        state.manifests.insert(name, manifest);
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

    /// [`Self::tool_definitions`] minus tools out of scope for a session on
    /// `channel` started by `trigger`: the manifest's `channels` axis (e.g.
    /// owner-only deck tools) and the tool's [`Tool::trigger_scope`] axis (e.g.
    /// `report_nothing`, visible only in a recurring cron fire). This is what
    /// the agent loop sends the LLM: a session's channel *and* trigger are both
    /// fixed for its whole life, so the filtered list stays byte-stable within a
    /// session and the prompt-cache constraint above still holds. A tool with no
    /// manifest is treated as channel-unrestricted (defensive — `register`
    /// always stores one).
    pub fn tool_definitions_for_session(&self, scope: SessionToolScope<'_>) -> Vec<ToolDefinition> {
        let mut defs: HashMap<String, ToolDefinition> = HashMap::new();
        for (name, tool) in &self.builtin {
            if scope.allows(self.builtin_manifests.get(name), tool.as_ref()) {
                let def = tool_definition_for(tool.as_ref());
                defs.insert(def.name.clone(), def);
            }
        }
        let dynamic = self.dynamic.read();
        for (name, tool) in &dynamic.tools {
            if scope.allows(dynamic.manifests.get(name), tool.as_ref()) {
                let def = tool_definition_for(tool.as_ref());
                defs.insert(def.name.clone(), def);
            } else {
                // A restricted dynamic tool must not fall back to a
                // same-named builtin it shadows.
                defs.remove(name);
            }
        }
        let mut out: Vec<ToolDefinition> = defs.into_values().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Names of every deferred tool this session may load — the roster it is
    /// shown. Sorted, so the message is byte-stable and a re-render differs
    /// only when the set genuinely differs.
    ///
    /// Deliberately ignores what is *already* loaded, even though the scope
    /// carries it. Filtering loaded tools out would shrink the roster on every
    /// load, and each shrink is a delta message the model gains nothing from —
    /// it can see those tools in its own tool list. Holding the set stable
    /// means a load produces no roster traffic at all, and re-selecting a tool
    /// whose definition scrolled out of a compacted transcript still works.
    pub fn deferred_tool_names(&self, scope: SessionToolScope<'_>) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        let mut push = |name: &String, manifest: Option<&ToolManifest>, tool: &dyn Tool| {
            if manifest.is_some_and(|m| m.deferred) && scope.allows_ignoring_load(manifest, tool) {
                names.push(name.clone());
            }
        };
        for (name, tool) in &self.builtin {
            push(name, self.builtin_manifests.get(name), tool.as_ref());
        }
        let dynamic = self.dynamic.read();
        for (name, tool) in &dynamic.tools {
            push(name, dynamic.manifests.get(name), tool.as_ref());
        }
        names.sort();
        names.dedup();
        names
    }

    /// The definition of one deferred tool, if this session may load it.
    /// `None` when the name is unknown or out of scope — `ToolSearch` reports
    /// both the same way, so naming a channel-restricted tool reveals nothing
    /// beyond what the roster already omits.
    pub fn loadable_definition(
        &self,
        name: &str,
        scope: SessionToolScope<'_>,
    ) -> Option<ToolDefinition> {
        let dynamic = self.dynamic.read();
        let (tool, manifest) = match dynamic.tools.get(name) {
            Some(tool) => (Arc::clone(tool), dynamic.manifests.get(name).cloned()),
            None => (
                Arc::clone(self.builtin.get(name)?),
                self.builtin_manifests.get(name).cloned(),
            ),
        };
        drop(dynamic);
        let deferred = manifest.as_ref().is_some_and(|m| m.deferred);
        (deferred && scope.allows_ignoring_load(manifest.as_ref(), tool.as_ref()))
            .then(|| tool_definition_for(tool.as_ref()))
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

    use super::SessionToolScope;
    use crate::LoadedTools;

    use baybo_storage::test_support::MemoryBlobStore;

    use super::ToolRegistry;
    use crate::ToolConcurrency;

    fn default_registry() -> ToolRegistry {
        let blob_store = Arc::new(MemoryBlobStore::new()) as Arc<dyn baybo_store::BlobStore>;
        ToolRegistry::with_defaults(crate::builtin::DefaultToolsConfig {
            blob_store,
            workspace_paths: baybo_workspace::WorkspacePaths::new("/tmp"),
            proxy: None,
            permission: Arc::new(crate::builtin::LivePermissionMode::new(
                crate::builtin::BashPermissionMode::Manual,
            )),
            builtin_memory: true,
        })
    }

    #[test]
    fn memory_delete_is_absent_when_builtin_memory_is_off() {
        // Nothing mentions a memory directory to the model with the feature
        // off, so a verb for tidying one would name a place its prompt never
        // describes.
        let blob_store = Arc::new(MemoryBlobStore::new()) as Arc<dyn baybo_store::BlobStore>;
        let registry = ToolRegistry::with_defaults(crate::builtin::DefaultToolsConfig {
            blob_store,
            workspace_paths: baybo_workspace::WorkspacePaths::new("/tmp"),
            proxy: None,
            permission: Arc::new(crate::builtin::LivePermissionMode::new(
                crate::builtin::BashPermissionMode::Manual,
            )),
            builtin_memory: false,
        });
        assert!(registry.get_manifest("MemoryDelete").is_none());
        assert!(default_registry().get_manifest("MemoryDelete").is_some());
    }

    #[test]
    fn defaults_register_blob_delivery_tools() {
        let registry = default_registry();

        assert!(registry.get("AttachFile").is_some());
        assert!(registry.get_manifest("AttachFile").is_some());
        assert!(registry.get("PutBlob").is_some());
        assert!(registry.get_manifest("PutBlob").is_some());
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
        for name in [
            "Write",
            "Edit",
            "Bash",
            "AttachFile",
            "PutBlob",
            "SecretAdd",
        ] {
            assert_eq!(
                registry.concurrency(name),
                ToolConcurrency::Exclusive,
                "{name} must stay exclusive"
            );
        }
    }

    /// A deferred tool is absent from a session's list until that session
    /// loads it, and present after — while the unfiltered listing carries it
    /// either way. Registered here rather than leaning on whatever
    /// `default_tools` happens to defer: which families are deferred is a
    /// product decision, and a test that encodes it fails on every retune.
    #[test]
    fn a_deferred_tool_appears_only_once_loaded() {
        let mut registry = default_registry();
        let tool = crate::test_support::EchoTool::new("Deferred");
        let mut manifest = tool.manifest();
        manifest.deferred = true;
        registry.register(Arc::new(tool), manifest);

        let names = |loaded: &LoadedTools| {
            registry
                .tool_definitions_for_session(SessionToolScope {
                    channel: &baybo_model::ChannelType::owner(),
                    trigger: &baybo_model::TriggerSource::User,
                    loaded,
                })
                .into_iter()
                .map(|d| d.name)
                .collect::<Vec<_>>()
        };

        let unloaded = LoadedTools::default();
        assert!(!names(&unloaded).contains(&"Deferred".to_string()));
        assert_eq!(
            registry.deferred_tool_names(SessionToolScope {
                channel: &baybo_model::ChannelType::owner(),
                trigger: &baybo_model::TriggerSource::User,
                loaded: &unloaded,
            }),
            vec!["Deferred".to_string()],
            "an unloaded deferred tool is what the roster advertises"
        );

        let loaded: LoadedTools = ["Deferred"].into_iter().collect();
        assert!(names(&loaded).contains(&"Deferred".to_string()));

        // Never hidden from the unfiltered listing — that one answers "what
        // does this deployment have", not "what can this session call".
        assert!(
            registry
                .tool_definitions()
                .iter()
                .any(|d| d.name == "Deferred")
        );
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

    #[test]
    fn channel_filter_hides_restricted_tools_and_keeps_the_rest() {
        let mut registry = default_registry();
        let echo = crate::test_support::EchoTool::new("OwnerOnly");
        let mut manifest = echo.manifest();
        manifest.channels = vec![baybo_model::ChannelType::owner()];
        registry.register(Arc::new(echo), manifest);

        let names =
            |defs: Vec<crate::ToolDefinition>| defs.into_iter().map(|d| d.name).collect::<Vec<_>>();

        let owner = names(registry.tool_definitions_for_session(SessionToolScope {
            channel: &baybo_model::ChannelType::owner(),
            trigger: &baybo_model::TriggerSource::User,
            loaded: &LoadedTools::default(),
        }));
        assert!(owner.contains(&"OwnerOnly".to_string()));
        assert!(owner.contains(&"PutBlob".to_string()));

        let telegram = names(registry.tool_definitions_for_session(SessionToolScope {
            channel: &baybo_model::ChannelType::telegram(),
            trigger: &baybo_model::TriggerSource::User,
            loaded: &LoadedTools::default(),
        }));
        assert!(!telegram.contains(&"OwnerOnly".to_string()));
        assert!(!telegram.contains(&"PutBlob".to_string()));
        // Unrestricted tools are unaffected, and the two lists differ by
        // exactly the two restricted entries.
        assert!(telegram.contains(&"AttachFile".to_string()));
        assert_eq!(owner.len(), telegram.len() + 2);
        // The unfiltered listing carries everything, this fixture defers
        // nothing, so the two agree.
        assert_eq!(registry.tool_definitions().len(), owner.len());
    }

    #[test]
    fn trigger_scope_shows_cron_fire_tools_only_to_a_recurring_fire() {
        use crate::{Tool, ToolContext, ToolOutput, ToolTriggerScope};
        use baybo_model::TriggerSource;

        struct FireOnly;
        #[async_trait::async_trait]
        impl Tool for FireOnly {
            fn name(&self) -> &str {
                "report_nothing"
            }
            fn description(&self) -> String {
                "x".into()
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            fn trigger_scope(&self) -> ToolTriggerScope {
                ToolTriggerScope::CronFire
            }
            async fn execute(
                &self,
                _p: serde_json::Value,
                _c: &ToolContext,
            ) -> crate::Result<ToolOutput> {
                Ok(ToolOutput::Text(String::new()))
            }
        }

        let mut registry = default_registry();
        let manifest = crate::ToolManifest {
            name: "report_nothing".into(),
            description: "x".into(),
            trust_level: baybo_model::TrustLevel::Trusted,
            parameters_schema: serde_json::json!({"type": "object"}),
            capabilities: vec![],
            channels: Vec::new(),
            deferred: false,
        };
        registry.register(Arc::new(FireOnly), manifest);

        let has = |trigger: &TriggerSource| {
            registry
                .tool_definitions_for_session(SessionToolScope {
                    channel: &baybo_model::ChannelType::owner(),
                    trigger,
                    loaded: &LoadedTools::default(),
                })
                .into_iter()
                .any(|d| d.name == "report_nothing")
        };
        let cron = |conversation: bool| TriggerSource::Cron {
            cron_job_id: "cj".into(),
            origin_session_id: None,
            conversation,
            job_title: None,
        };

        // A recurring fire (its own conversation) sees it; an ordinary session
        // and a one-shot fire's private workspace do not.
        assert!(has(&cron(true)));
        assert!(!has(&TriggerSource::User));
        assert!(!has(&cron(false)));
    }
}
