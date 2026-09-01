use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use serde_json::Value;

use crate::{Tool, ToolConcurrency, ToolContext, ToolDefinition, ToolManifest, ToolOutput};

pub struct ToolRegistry {
    builtin: HashMap<String, Arc<dyn Tool>>,
    builtin_manifests: HashMap<String, ToolManifest>,
    dynamic: Arc<RwLock<DynamicState>>,
}

#[derive(Default)]
struct DynamicState {
    tools: HashMap<String, Arc<dyn Tool>>,
    manifests: HashMap<String, ToolManifest>,
    by_source: HashMap<String, Vec<String>>,
    /// Names whose schemas are withheld from the LLM tool block
    /// (`tool_definitions_for_session`), discovered via `ToolSearch` and
    /// called via `ToolInvoke`. Registration state, not governance: a
    /// deferred tool resolves and executes exactly like an eager one —
    /// the executor's doors never consult this set.
    deferred: std::collections::HashSet<String>,
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
            dynamic: Arc::new(RwLock::new(DynamicState::default())),
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

    /// Whether `name` is registered with its schema withheld from the LLM
    /// tool block. The agent loop's `ToolInvoke` unwrap consults this so an
    /// un-namespaced deferred registration (the cron/deck builtins) is a
    /// valid envelope target while eager builtins are not.
    pub fn is_deferred(&self, name: &str) -> bool {
        self.dynamic.read().deferred.contains(name)
    }

    /// Canonical registered name for a `ToolInvoke` target: the exact
    /// deferred name, or — because `ToolSearch` groups by SOURCE and the
    /// model naturally spells that as `source/Name` — the alias
    /// `cron/CronCreate` → `CronCreate` when the bare name is deferred and
    /// registered under that source. Data-driven off `by_source`: any
    /// un-namespaced deferred batch gets the tolerance, nothing is
    /// hardcoded, and a bare name deferred under a DIFFERENT source does
    /// not alias. `None` when nothing deferred matches.
    pub fn resolve_deferred_target(&self, name: &str) -> Option<String> {
        canonical_deferred_name(&self.dynamic.read(), name)
    }

    /// A live, narrow view over the deferred half of the dynamic registry
    /// for `ToolSearch`. Shares only the dynamic-state lock (no `Arc` cycle
    /// back to the whole registry) and follows reconciler
    /// connects/disconnects live.
    pub fn deferred_tool_index(&self) -> DeferredToolIndex {
        DeferredToolIndex {
            dynamic: Arc::clone(&self.dynamic),
        }
    }

    /// Register a tool sourced from an external provider (an MCP server,
    /// for example). `source` is the logical owner the reconciler uses
    /// for [`Self::unregister_for_source`]; tools should be named with a
    /// `<source>/<tool>` convention to avoid colliding with builtins.
    pub fn register_dynamic(&self, source: &str, tool: Arc<dyn Tool>, manifest: ToolManifest) {
        self.register_dynamic_inner(source, tool, manifest, false);
    }

    /// [`Self::register_dynamic`], but with the tool's schema withheld from
    /// the LLM tool block. The tool still resolves and executes by name;
    /// sessions reach it through `ToolSearch` + `ToolInvoke`.
    pub fn register_dynamic_deferred(
        &self,
        source: &str,
        tool: Arc<dyn Tool>,
        manifest: ToolManifest,
    ) {
        self.register_dynamic_inner(source, tool, manifest, true);
    }

    fn register_dynamic_inner(
        &self,
        source: &str,
        tool: Arc<dyn Tool>,
        manifest: ToolManifest,
        deferred: bool,
    ) {
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
        if deferred {
            state.deferred.insert(name.clone());
        } else {
            // A re-registration under the same name flips the bit both ways.
            state.deferred.remove(&name);
        }
        state
            .by_source
            .entry(source.to_string())
            .or_default()
            .push(name);
    }

    /// Atomically make `tools` the complete set registered under `source`,
    /// returning the names registered afterwards.
    ///
    /// Not [`Self::unregister_for_source`] followed by
    /// [`Self::register_dynamic`]: those take the write lock twice, and a
    /// lookup landing in the gap resolves to `NotFound` for a tool that is
    /// about to exist again. A consumer that republishes its whole set on
    /// every config reload — including reloads that change nothing about it —
    /// would hit that gap routinely rather than rarely.
    pub fn replace_source(
        &self,
        source: &str,
        tools: Vec<(Arc<dyn Tool>, ToolManifest)>,
    ) -> Vec<String> {
        let mut state = self.dynamic.write();
        for name in state.by_source.remove(source).unwrap_or_default() {
            state.tools.remove(&name);
            state.manifests.remove(&name);
            state.deferred.remove(&name);
        }
        let mut names = Vec::with_capacity(tools.len());
        for (tool, manifest) in tools {
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
            state.tools.insert(name.clone(), tool);
            state.manifests.insert(name.clone(), manifest);
            names.push(name);
        }
        if names.is_empty() {
            state.by_source.remove(source);
        } else {
            state.by_source.insert(source.to_string(), names.clone());
        }
        names
    }

    /// Drop every dynamic tool previously registered under `source`.
    /// Returns the names that were removed.
    pub fn unregister_for_source(&self, source: &str) -> Vec<String> {
        let mut state = self.dynamic.write();
        let names = state.by_source.remove(source).unwrap_or_default();
        for name in &names {
            state.tools.remove(name);
            state.manifests.remove(name);
            state.deferred.remove(name);
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
    /// the agent loop sends the LLM. A tool with no manifest is treated as
    /// channel-unrestricted (defensive — `register` always stores one).
    ///
    /// All three axes below are fixed for a session, and it is tempting to
    /// conclude the answer is too. It is not: the axes are fixed, the set they
    /// filter is not. The MCP reconciler adds and drops dynamic tools
    /// underneath live sessions (`register_dynamic` / `unregister_for_source`),
    /// and the agent loop calls this afresh for every request, so a server
    /// connecting or dropping changes the tool block **mid-session, mid-turn**
    /// — including between one request and its own compaction, which F2 went
    /// out of its way to give a shared prefix.
    ///
    /// So the prompt-cache constraint above is a best effort here, not an
    /// invariant. Measured over 16 days of board runs: 17 mid-session changes,
    /// of which 5 were real shrinkages with a live turn inside the window. The
    /// upside is that a server the user adds mid-conversation reaches that
    /// conversation immediately. Snapshotting per turn would buy the invariant
    /// and keep that upside; it is deliberately not done, because after the
    /// `SharedWorkspace` scope took `browser/*` out of issue runs, the churn
    /// that was actually measured is gone and the rest is unquantified.
    ///
    /// `allowlist` is the third axis and the only one that is not a boundary:
    /// a subagent profile naming the tools its child needs (`Some`) keeps the
    /// rest out of the child's prefix, which is a budget, not a gate — the
    /// executor still runs a name that arrives anyway, exactly as it would
    /// today. `None` leaves the other two axes to decide. Unlike them it never
    /// changes under a session at all: it is written once, at spawn, before
    /// the child's actor exists.
    pub fn tool_definitions_for_session(
        &self,
        channel: &baybo_model::ChannelType,
        trigger: &baybo_model::TriggerSource,
        allowlist: Option<&[String]>,
    ) -> Vec<ToolDefinition> {
        let allowed = |name: &String| allowlist.is_none_or(|list| list.contains(name));
        let mut defs: HashMap<String, ToolDefinition> = HashMap::new();
        for (name, tool) in &self.builtin {
            let channel_ok = self
                .builtin_manifests
                .get(name)
                .is_none_or(|m| m.allows_channel(channel));
            if channel_ok && tool.trigger_scope().allows_trigger(trigger) && allowed(name) {
                let def = tool_definition_offered(tool.as_ref(), trigger);
                defs.insert(def.name.clone(), def);
            }
        }
        let dynamic = self.dynamic.read();
        for (name, tool) in &dynamic.tools {
            let channel_ok = dynamic
                .manifests
                .get(name)
                .is_none_or(|m| m.allows_channel(channel));
            // A deferred tool is advertised only when a subagent profile's
            // allowlist names it explicitly: the author asked for that
            // schema, and child prompts are small. Everyone else reaches it
            // through `ToolSearch` + `ToolInvoke`.
            let withheld = dynamic.deferred.contains(name)
                && !allowlist.is_some_and(|list| list.contains(name));
            if channel_ok
                && tool.trigger_scope().allows_trigger(trigger)
                && allowed(name)
                && !withheld
            {
                let def = tool_definition_offered(tool.as_ref(), trigger);
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

    /// Resolve a tool and its manifest from one registry snapshot. Dynamic
    /// registrations can be replaced while a call is in flight, so security
    /// checks and execution must pin this pair rather than performing several
    /// name lookups that could observe different server generations.
    pub fn get_with_manifest(&self, name: &str) -> Option<(Arc<dyn Tool>, Option<ToolManifest>)> {
        let dynamic = self.dynamic.read();
        if let Some(tool) = dynamic.tools.get(name) {
            return Some((Arc::clone(tool), dynamic.manifests.get(name).cloned()));
        }
        self.builtin
            .get(name)
            .cloned()
            .map(|tool| (tool, self.builtin_manifests.get(name).cloned()))
    }

    /// Look up a tool by name. Dynamic registrations shadow builtins.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.get_with_manifest(name).map(|(tool, _)| tool)
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

/// Narrow, cloneable view over the deferred half of the dynamic registry,
/// held by the `ToolSearch` builtin. What it reveals to a session is
/// filtered on the same two doors as prompt assembly (`allows_channel` +
/// `trigger_scope`), and schemas go through [`tool_definition_offered`] so a
/// trigger-withheld parameter stays withheld in search results too.
#[derive(Clone)]
pub struct DeferredToolIndex {
    dynamic: Arc<RwLock<DynamicState>>,
}

/// Where a name landed when [`DeferredToolIndex::lookup`] resolved it, so
/// `ToolSearch` can answer misses precisely instead of a blanket
/// "not found".
pub enum DeferredLookup {
    /// Deferred and visible to this session: full definition attached.
    Visible(ToolDefinition),
    /// Deferred, but this session's channel or trigger scope excludes it.
    OutOfScope,
    /// Registered eagerly — it is already in the session's tool block.
    Eager,
    /// No dynamic tool has this name (never configured, or its server is
    /// currently disconnected).
    Unknown,
}

impl DeferredToolIndex {
    /// Every deferred tool this session may see, name-sorted for stable
    /// output.
    pub fn visible(
        &self,
        channel: &baybo_model::ChannelType,
        trigger: &baybo_model::TriggerSource,
    ) -> Vec<ToolDefinition> {
        self.visible_with_source(channel, trigger)
            .into_iter()
            .map(|(_, def)| def)
            .collect()
    }

    /// [`Self::visible`] with each tool's registration SOURCE attached — the
    /// grouping key `ToolSearch`'s directory and `server` filter use. For an
    /// MCP tool the source equals the `server/` name prefix; for a deferred
    /// builtin batch (cron, deck) it is the batch label, which is what makes
    /// un-namespaced names groupable at all. Sorted by (source, name).
    pub fn visible_with_source(
        &self,
        channel: &baybo_model::ChannelType,
        trigger: &baybo_model::TriggerSource,
    ) -> Vec<(String, ToolDefinition)> {
        let dynamic = self.dynamic.read();
        let mut out: Vec<(String, ToolDefinition)> = Vec::new();
        for (source, names) in &dynamic.by_source {
            for name in names {
                if !dynamic.deferred.contains(name) {
                    continue;
                }
                let Some(tool) = dynamic.tools.get(name) else {
                    continue;
                };
                let channel_ok = dynamic
                    .manifests
                    .get(name)
                    .is_none_or(|m| m.allows_channel(channel));
                if channel_ok && tool.trigger_scope().allows_trigger(trigger) {
                    out.push((
                        source.clone(),
                        tool_definition_offered(tool.as_ref(), trigger),
                    ));
                }
            }
        }
        out.sort_by(|a, b| (&a.0, &a.1.name).cmp(&(&b.0, &b.1.name)));
        out
    }

    /// Resolve one exact name against a single registry snapshot. The
    /// channel/trigger doors are evaluated before the eager/deferred split:
    /// an eager tool this session cannot see answers `OutOfScope`, not
    /// "call it directly" — the direct call would only hit the executor's
    /// refusal. A `source/Name` spelling of an un-namespaced deferred
    /// registration resolves through the same alias `ToolInvoke` accepts.
    pub fn lookup(
        &self,
        name: &str,
        channel: &baybo_model::ChannelType,
        trigger: &baybo_model::TriggerSource,
    ) -> DeferredLookup {
        let dynamic = self.dynamic.read();
        let canonical = if dynamic.tools.contains_key(name) {
            None
        } else {
            canonical_deferred_name(&dynamic, name)
        };
        let name = canonical.as_deref().unwrap_or(name);
        let Some(tool) = dynamic.tools.get(name) else {
            return DeferredLookup::Unknown;
        };
        let channel_ok = dynamic
            .manifests
            .get(name)
            .is_none_or(|m| m.allows_channel(channel));
        if !(channel_ok && tool.trigger_scope().allows_trigger(trigger)) {
            return DeferredLookup::OutOfScope;
        }
        if !dynamic.deferred.contains(name) {
            return DeferredLookup::Eager;
        }
        DeferredLookup::Visible(tool_definition_offered(tool.as_ref(), trigger))
    }
}

/// See [`ToolRegistry::resolve_deferred_target`]. Split out so the
/// registry method and [`DeferredToolIndex::lookup`] share one rule.
fn canonical_deferred_name(state: &DynamicState, requested: &str) -> Option<String> {
    if state.deferred.contains(requested) {
        return Some(requested.to_string());
    }
    let (source, bare) = requested.split_once('/')?;
    (state.deferred.contains(bare)
        && state
            .by_source
            .get(source)
            .is_some_and(|names| names.iter().any(|n| n == bare)))
    .then(|| bare.to_string())
}

/// A tool's whole declared surface, for inventory views (`baybo status`, the
/// admin tools listing). Not what a session is offered — see
/// [`tool_definition_offered`].
fn tool_definition_for(tool: &dyn Tool) -> ToolDefinition {
    ToolDefinition {
        name: tool.name().to_string(),
        description: tool.description(),
        parameters_schema: tool.parameters_schema(),
    }
}

/// What a session started by `trigger` is actually handed: the surface minus
/// any parameter [`Tool::parameters_schema_for`] withholds from it.
fn tool_definition_offered(
    tool: &dyn Tool,
    trigger: &baybo_model::TriggerSource,
) -> ToolDefinition {
    ToolDefinition {
        name: tool.name().to_string(),
        description: tool.description(),
        parameters_schema: tool.parameters_schema_for(trigger),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use baybo_storage::test_support::MemoryBlobStore;

    use super::ToolRegistry;
    use crate::ToolConcurrency;

    fn default_registry() -> ToolRegistry {
        let blob_store = Arc::new(MemoryBlobStore::new()) as Arc<dyn baybo_store::BlobStore>;
        ToolRegistry::with_defaults(crate::builtin::DefaultToolsConfig {
            blob_store,
            process_manager: baybo_process::ProcessManager::transient(),
            workspace_paths: baybo_workspace::WorkspacePaths::new("/tmp"),
            proxy: None,
            permission: Arc::new(crate::builtin::LivePermissionMode::new(
                crate::builtin::PermissionMode::Manual,
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
            process_manager: baybo_process::ProcessManager::transient(),
            workspace_paths: baybo_workspace::WorkspacePaths::new("/tmp"),
            proxy: None,
            permission: Arc::new(crate::builtin::LivePermissionMode::new(
                crate::builtin::PermissionMode::Manual,
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
        assert!(registry.get("GetBlob").is_some());
        assert!(registry.get_manifest("GetBlob").is_some());
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
            "GetBlob",
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

        let owner = names(registry.tool_definitions_for_session(
            &baybo_model::ChannelType::owner(),
            &baybo_model::TriggerSource::User,
            None,
        ));
        assert!(owner.contains(&"OwnerOnly".to_string()));
        assert!(owner.contains(&"PutBlob".to_string()));

        let telegram = names(registry.tool_definitions_for_session(
            &baybo_model::ChannelType::telegram(),
            &baybo_model::TriggerSource::User,
            None,
        ));
        assert!(!telegram.contains(&"OwnerOnly".to_string()));
        assert!(!telegram.contains(&"PutBlob".to_string()));
        // Unrestricted tools are unaffected, and the two lists differ by
        // exactly the two restricted entries.
        assert!(telegram.contains(&"AttachFile".to_string()));
        assert_eq!(owner.len(), telegram.len() + 2);
        // The unfiltered listing still carries everything.
        assert_eq!(registry.tool_definitions().len(), owner.len());
    }

    #[test]
    fn trigger_scope_shows_cron_tools_only_to_a_recurring_conversation() {
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
                ToolTriggerScope::CronConversation
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
        };
        registry.register(Arc::new(FireOnly), manifest);

        let has = |trigger: &TriggerSource| {
            registry
                .tool_definitions_for_session(&baybo_model::ChannelType::owner(), trigger, None)
                .into_iter()
                .any(|d| d.name == "report_nothing")
        };
        let cron = |conversation: bool| TriggerSource::Cron {
            cron_job_id: "cj".into(),
            origin_session_id: None,
            conversation,
            job_title: None,
            project_id: None,
        };

        // A recurring fire (its own conversation) sees it; an ordinary session
        // and a one-shot fire's private workspace do not.
        assert!(has(&cron(true)));
        assert!(!has(&TriggerSource::User));
        assert!(!has(&cron(false)));
    }

    #[test]
    fn trigger_scope_shows_board_tools_only_to_project_linked_sessions() {
        use crate::{Tool, ToolContext, ToolOutput, ToolTriggerScope};
        use baybo_model::TriggerSource;

        struct BoardOnly;
        #[async_trait::async_trait]
        impl Tool for BoardOnly {
            fn name(&self) -> &str {
                "IssueList"
            }
            fn description(&self) -> String {
                "x".into()
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            fn trigger_scope(&self) -> ToolTriggerScope {
                ToolTriggerScope::ProjectBoard
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
        registry.register(
            Arc::new(BoardOnly),
            crate::ToolManifest {
                name: "IssueList".into(),
                description: "x".into(),
                trust_level: baybo_model::TrustLevel::Trusted,
                parameters_schema: serde_json::json!({"type": "object"}),
                capabilities: vec![],
                channels: Vec::new(),
            },
        );

        let has = |trigger: &TriggerSource| {
            registry
                .tool_definitions_for_session(&baybo_model::ChannelType::owner(), trigger, None)
                .into_iter()
                .any(|d| d.name == "IssueList")
        };
        assert!(has(&TriggerSource::Issue {
            project_id: baybo_model::ProjectId::generate(),
            issue_id: baybo_model::IssueId::generate(),
            number: 1,
        }));
        assert!(has(&TriggerSource::Cron {
            cron_job_id: "cj".into(),
            origin_session_id: None,
            conversation: true,
            job_title: None,
            project_id: Some(baybo_model::ProjectId::generate()),
        }));
        assert!(!has(&TriggerSource::User));
        assert!(!has(&TriggerSource::Cron {
            cron_job_id: "cj".into(),
            origin_session_id: None,
            conversation: true,
            job_title: None,
            project_id: None,
        }));
    }

    /// The allowlist is a third axis over the other two, not a replacement:
    /// a name on the list that the trigger already refuses stays refused.
    #[test]
    fn a_profile_s_tool_list_narrows_what_the_other_axes_already_allow() {
        use crate::{Tool, ToolContext, ToolOutput, ToolTriggerScope};
        use baybo_model::TriggerSource;

        struct Named(&'static str, ToolTriggerScope);
        #[async_trait::async_trait]
        impl Tool for Named {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> String {
                "x".into()
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            fn trigger_scope(&self) -> ToolTriggerScope {
                self.1
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
        for (name, scope) in [
            ("Keeper", ToolTriggerScope::Any),
            ("Dropped", ToolTriggerScope::Any),
            ("OffTrigger", ToolTriggerScope::SharedWorkspace),
        ] {
            registry.register(
                Arc::new(Named(name, scope)),
                crate::ToolManifest {
                    name: name.into(),
                    description: "x".into(),
                    trust_level: baybo_model::TrustLevel::Trusted,
                    parameters_schema: serde_json::json!({"type": "object"}),
                    capabilities: vec![],
                    channels: Vec::new(),
                },
            );
        }

        let run = TriggerSource::Issue {
            project_id: baybo_model::ProjectId::generate(),
            issue_id: baybo_model::IssueId::generate(),
            number: 1,
        };
        let names = |allow: Option<&[String]>| {
            registry
                .tool_definitions_for_session(&baybo_model::ChannelType::owner(), &run, allow)
                .into_iter()
                .map(|d| d.name)
                .collect::<Vec<_>>()
        };

        let unrestricted = names(None);
        assert!(unrestricted.contains(&"Keeper".to_string()));
        assert!(unrestricted.contains(&"Dropped".to_string()));

        let allow = ["Keeper".to_string(), "OffTrigger".to_string()];
        let restricted = names(Some(&allow));
        assert!(restricted.contains(&"Keeper".to_string()));
        assert!(
            !restricted.contains(&"Dropped".to_string()),
            "a tool the list does not name is not offered"
        );
        assert!(
            !restricted.contains(&"OffTrigger".to_string()),
            "naming a tool cannot hand a run one its trigger refuses"
        );
    }

    /// A deferred dynamic tool is registered — it resolves and executes —
    /// but its schema stays out of the session tool block until a subagent
    /// allowlist names it explicitly.
    #[test]
    fn deferred_dynamic_tools_are_withheld_from_the_session_block() {
        use baybo_model::TriggerSource;

        use crate::{Tool, ToolContext, ToolOutput, ToolTriggerScope};

        struct Named(&'static str);

        #[async_trait::async_trait]
        impl Tool for Named {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> String {
                "x".into()
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            fn trigger_scope(&self) -> ToolTriggerScope {
                ToolTriggerScope::Any
            }
            async fn execute(
                &self,
                _p: serde_json::Value,
                _c: &ToolContext,
            ) -> crate::Result<ToolOutput> {
                Ok(ToolOutput::Text(String::new()))
            }
        }

        let registry = default_registry();
        let manifest = |name: &str| crate::ToolManifest {
            name: name.into(),
            description: "x".into(),
            trust_level: baybo_model::TrustLevel::Trusted,
            parameters_schema: serde_json::json!({"type": "object"}),
            capabilities: vec![],
            channels: Vec::new(),
        };
        registry.register_dynamic_deferred(
            "srv",
            Arc::new(Named("srv/deferred_op")),
            manifest("srv/deferred_op"),
        );
        registry.register_dynamic(
            "srv",
            Arc::new(Named("srv/eager_op")),
            manifest("srv/eager_op"),
        );

        let names = |allow: Option<&[String]>| {
            registry
                .tool_definitions_for_session(
                    &baybo_model::ChannelType::owner(),
                    &TriggerSource::User,
                    allow,
                )
                .into_iter()
                .map(|d| d.name)
                .collect::<Vec<_>>()
        };

        let advertised = names(None);
        assert!(advertised.contains(&"srv/eager_op".to_string()));
        assert!(
            !advertised.contains(&"srv/deferred_op".to_string()),
            "a deferred tool's schema must stay out of the block"
        );
        // ...while staying fully registered for execution.
        assert!(registry.get("srv/deferred_op").is_some());
        assert!(registry.get_manifest("srv/deferred_op").is_some());

        // An allowlist naming the deferred tool pins it eager: the profile
        // author asked for that schema by name.
        let allow = ["srv/deferred_op".to_string()];
        let pinned = names(Some(&allow));
        assert!(pinned.contains(&"srv/deferred_op".to_string()));
        assert!(!pinned.contains(&"srv/eager_op".to_string()));

        // Disconnect cleans the deferred set with the registration.
        registry.unregister_for_source("srv");
        assert!(registry.get("srv/deferred_op").is_none());
        let after = names(None);
        assert!(!after.contains(&"srv/deferred_op".to_string()));

        // A re-registration under the same name without deferral flips the
        // bit back: the eager registration must advertise.
        registry.register_dynamic_deferred(
            "srv",
            Arc::new(Named("srv/flip")),
            manifest("srv/flip"),
        );
        registry.register_dynamic("srv", Arc::new(Named("srv/flip")), manifest("srv/flip"));
        assert!(names(None).contains(&"srv/flip".to_string()));
    }

    /// The tools whose target is state the workspace shares, or a chat there
    /// is nobody at, stay out of a card's run. Free-to-construct ones are
    /// pinned here; the deck, skill and attachment tools carry the same scope
    /// but need a manager to build, so their own crates hold that assertion.
    #[test]
    fn a_run_is_not_offered_the_tools_that_act_outside_it() {
        use crate::{Tool, ToolTriggerScope};
        use baybo_model::TriggerSource;

        let run = TriggerSource::Issue {
            project_id: baybo_model::ProjectId::generate(),
            issue_id: baybo_model::IssueId::generate(),
            number: 1,
        };
        for tool in [
            Arc::new(crate::builtin::secret::SecretAddTool) as Arc<dyn Tool>,
            Arc::new(crate::builtin::echo::EchoTool) as Arc<dyn Tool>,
        ] {
            assert_eq!(
                tool.trigger_scope(),
                ToolTriggerScope::SharedWorkspace,
                "{} writes state every session shares",
                tool.name()
            );
            assert!(!tool.trigger_scope().allows_trigger(&run));
        }
    }

    /// A **dynamic** tool — the shape every MCP server registers under — is
    /// filtered on the same axis as a builtin. This is what keeps the browser
    /// server's 24 tools out of a card's run, and only out of a card's run.
    #[test]
    fn a_shared_workspace_tool_reaches_every_session_but_a_card_s_run() {
        use crate::{Tool, ToolContext, ToolOutput, ToolTriggerScope};
        use baybo_model::TriggerSource;

        struct WatchedOnly;
        #[async_trait::async_trait]
        impl Tool for WatchedOnly {
            fn name(&self) -> &str {
                "browser/navigate_page"
            }
            fn description(&self) -> String {
                "x".into()
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            fn trigger_scope(&self) -> ToolTriggerScope {
                ToolTriggerScope::SharedWorkspace
            }
            async fn execute(
                &self,
                _p: serde_json::Value,
                _c: &ToolContext,
            ) -> crate::Result<ToolOutput> {
                Ok(ToolOutput::Text(String::new()))
            }
        }

        let registry = default_registry();
        registry.register_dynamic(
            "browser",
            Arc::new(WatchedOnly),
            crate::ToolManifest {
                name: "browser/navigate_page".into(),
                description: "x".into(),
                trust_level: baybo_model::TrustLevel::Trusted,
                parameters_schema: serde_json::json!({"type": "object"}),
                capabilities: vec![],
                channels: Vec::new(),
            },
        );

        let has = |trigger: &TriggerSource| {
            registry
                .tool_definitions_for_session(&baybo_model::ChannelType::owner(), trigger, None)
                .into_iter()
                .any(|d| d.name == "browser/navigate_page")
        };
        assert!(has(&TriggerSource::User));
        assert!(
            has(&TriggerSource::Cron {
                cron_job_id: "cj".into(),
                origin_session_id: None,
                conversation: true,
                job_title: None,
                project_id: None,
            }),
            "a cron fire may browse"
        );
        assert!(
            has(&TriggerSource::Cron {
                cron_job_id: "cj".into(),
                origin_session_id: None,
                conversation: false,
                job_title: None,
                project_id: Some(baybo_model::ProjectId::generate()),
            }),
            "a board-patrol fire carries a project id and is still not a card's run"
        );
        assert!(
            !has(&TriggerSource::Issue {
                project_id: baybo_model::ProjectId::generate(),
                issue_id: baybo_model::IssueId::generate(),
                number: 1,
            }),
            "a card's run has its own checkout; the shared browser is not its to hold"
        );
    }

    fn recording(name: &str) -> (Arc<dyn crate::Tool>, crate::ToolManifest) {
        let tool = crate::test_support::RecordingTool::new(name);
        let manifest = tool.manifest();
        (Arc::new(tool), manifest)
    }

    #[test]
    fn replace_source_swaps_the_whole_set() {
        let registry = default_registry();
        registry.replace_source("s", vec![recording("a"), recording("b")]);
        assert!(registry.get("a").is_some());
        assert!(registry.get("b").is_some());

        // The new set is the whole set — `b` goes, `c` arrives, `a` stays.
        let names = registry.replace_source("s", vec![recording("a"), recording("c")]);
        assert_eq!(names, vec!["a".to_string(), "c".to_string()]);
        assert!(registry.get("a").is_some());
        assert!(registry.get("b").is_none(), "dropped tool still resolves");
        assert!(registry.get("c").is_some());
        assert!(registry.get_manifest("b").is_none());
    }

    /// Republishing the identical set must not accumulate `by_source`
    /// entries — a reload runs this on every trigger, changed or not.
    #[test]
    fn replace_source_is_idempotent() {
        let registry = default_registry();
        for _ in 0..3 {
            registry.replace_source("s", vec![recording("a")]);
        }
        assert_eq!(
            registry.dynamic_names_for_source("s"),
            vec!["a".to_string()]
        );
        assert_eq!(
            registry
                .tool_definitions()
                .iter()
                .filter(|d| d.name == "a")
                .count(),
            1
        );
    }

    #[test]
    fn replace_source_with_nothing_clears_the_source() {
        let registry = default_registry();
        registry.replace_source("s", vec![recording("a")]);
        assert!(registry.replace_source("s", Vec::new()).is_empty());
        assert!(registry.get("a").is_none());
        assert!(registry.dynamic_names_for_source("s").is_empty());
        // …and it leaves another source alone.
        registry.replace_source("other", vec![recording("z")]);
        registry.replace_source("s", Vec::new());
        assert!(registry.get("z").is_some());
    }
}

#[cfg(test)]
mod offered_schema_tests {
    use super::*;
    use baybo_model::TriggerSource;
    use baybo_storage::test_support::MemoryBlobStore;
    use std::sync::Arc;

    fn registry() -> ToolRegistry {
        ToolRegistry::with_defaults(crate::builtin::DefaultToolsConfig {
            blob_store: Arc::new(MemoryBlobStore::new()) as Arc<dyn baybo_store::BlobStore>,
            process_manager: baybo_process::ProcessManager::transient(),
            workspace_paths: baybo_workspace::WorkspacePaths::new("/tmp"),
            proxy: None,
            permission: Arc::new(crate::builtin::LivePermissionMode::new(
                crate::builtin::PermissionMode::Manual,
            )),
            builtin_memory: true,
        })
    }

    fn issue_run() -> TriggerSource {
        TriggerSource::Issue {
            project_id: baybo_model::ProjectId::generate(),
            issue_id: baybo_model::IssueId::generate(),
            number: 1,
        }
    }

    fn props(def: &ToolDefinition) -> Vec<String> {
        def.parameters_schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// A knob the runtime will refuse is not offered. `bash`'s `on_timeout`
    /// only chooses between detaching and killing, and without a background
    /// host there is nothing to detach into — the command is killed whatever
    /// the model picks.
    #[test]
    fn a_card_s_run_is_not_offered_a_knob_its_runtime_refuses() {
        let registry = registry();
        let owner = baybo_model::ChannelType::owner();

        let in_chat = registry.tool_definitions_for_session(&owner, &TriggerSource::User, None);
        let bash = in_chat
            .iter()
            .find(|d| d.name == "Bash")
            .expect("Bash is registered");
        assert!(
            props(bash).iter().any(|p| p == "on_timeout"),
            "a session that can host background work keeps the choice: {:?}",
            props(bash)
        );

        let in_run = registry.tool_definitions_for_session(&owner, &issue_run(), None);
        let bash = in_run
            .iter()
            .find(|d| d.name == "Bash")
            .expect("Bash is registered");
        assert!(
            !props(bash).iter().any(|p| p == "on_timeout"),
            "a card's run must not be offered it: {:?}",
            props(bash)
        );
        assert!(
            props(bash).iter().any(|p| p == "command"),
            "the rest of the schema survives: {:?}",
            props(bash)
        );
    }

    /// The full surface still exists — the manifest and the inventory views
    /// carry it, because the executor accepts a parameter this withholds.
    #[test]
    fn withholding_a_parameter_does_not_narrow_the_declared_surface() {
        let registry = registry();
        let all = registry.tool_definitions();
        let bash = all
            .iter()
            .find(|d| d.name == "Bash")
            .expect("Bash is registered");
        assert!(
            props(bash).iter().any(|p| p == "on_timeout"),
            "{:?}",
            props(bash)
        );
    }
}
