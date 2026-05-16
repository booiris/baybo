//! Shared runtime assembly for the chat-loop boot paths.
//!
//! `main.rs` (TUI) and `gateway_cmd::start` (HTTP gateway) both need the
//! same manager graph, router, and signal handler. Keeping the wiring
//! here lets each entry point stay focused on what is genuinely
//! different — TUI adds `TuiAdapter` + slash + dashboard, the
//! gateway adds `GatewayServer` and the channel TCP listener — while
//! the common backbone stays in one place.
//!
//! Contract:
//!
//! * [`build_managers`] constructs every `Arc<Manager>` the actor graph
//!   needs. Errors cover missing encryption keys, storage open failures,
//!   and LLM client configuration.
//! * [`wire_router`] glues the graph into a [`Router`] + [`AgentActor`]
//!   spawner, returning a [`RouterRunHandle`] with the message channels
//!   ready for driving.
//! * [`install_signal_handler`] wires SIGINT/SIGTERM (Unix) or Ctrl-C
//!   (non-Unix) into the shared [`ShutdownSignal`] carried by
//!   [`ManagerGraph`].

use std::sync::Arc;

use aura_agent::actor::AgentActor;
use aura_agent::agent_loop::{AgentLoop, AgentLoopConfig};
use aura_agent::router::Router;
use aura_agent::service::{ShutdownSignal, TaskTracker};
use aura_agent::session_log::SessionLlmLogger;
use aura_agent::soul::Soul;
use aura_agent::supervisor::AgentSupervisor;
use aura_agent::tool_executor::ToolExecutor;
use aura_agent::{
    CostManager, CronScheduler, CronTriggerEvent, JobLifecycle, MemoryManager, SecretVault,
    SecurityGateway, SessionManager, SpanRecorder, SpendingLimits, TraceEventStream,
};
use aura_channels::{AgentOutput, ChannelRegistry, IncomingMessage};
use aura_config::AuraConfig;
use aura_context::{ContextManager, ContextManagerConfig, TiktokenTokenizer, Tokenizer};
use aura_llm::GuardedLlm;
use aura_model::SystemSpawnRequest;
use aura_security::{EncryptionKey, LeakDetectionRule, LeakDetector};
use aura_skills::SkillRegistry;
use aura_skills_assessor::SkillAssessor;
use aura_storage::Store;
use aura_tools::ToolRegistry;
use aura_tools::mcp::{EmbeddedMcpServer, McpReconciler};
use aura_workspace::WorkspaceManager;
use regex::Regex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::boot;

/// Build a [`LeakDetector`] seeded from config, optionally registering
/// extra `LeakAction::Replace` rules for the listed gateway-owned
/// tokens so any log line that happens to echo them is masked on disk.
/// The same `Arc` is then shared between the tracing file redactor and
/// [`build_managers`], keeping redaction coverage consistent across
/// every log surface.
///
/// Each entry in `gateway_tokens` is a `(name, token)` tuple where
/// `name` becomes the rule name (used for diagnostics) and `token` is
/// the literal value to redact. Empty token strings are skipped.
pub fn build_leak_detector(
    security: &aura_config::SecurityConfig,
    gateway_tokens: &[(&str, &str)],
) -> Arc<LeakDetector> {
    let mut detector = boot::build_leak_detector(security);
    for (name, token) in gateway_tokens {
        if token.is_empty() {
            continue;
        }
        match Regex::new(&regex::escape(token)) {
            Ok(pattern) => detector.add_rule(LeakDetectionRule {
                name: (*name).into(),
                pattern,
                action: aura_security::LeakAction::Replace,
            }),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    rule = %name,
                    "failed to compile gateway token redaction rule",
                );
            }
        }
    }
    Arc::new(detector)
}

/// Load the master encryption key, falling back to a well-known dev
/// key when `AURA_ALLOW_DEV_ENCRYPTION_KEY=1`. The full manager graph
/// and the gateway's vault-only subcommands (`enable`, `token show`,
/// `token rotate`) both need this exact policy — keeping the dev-key
/// gate in one place prevents them from drifting.
pub fn load_master_key(security: &aura_config::SecurityConfig) -> anyhow::Result<EncryptionKey> {
    match boot::load_encryption_key(security) {
        Ok(k) => Ok(k),
        Err(e) => {
            let allow_dev = std::env::var("AURA_ALLOW_DEV_ENCRYPTION_KEY")
                .ok()
                .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
            if !allow_dev {
                return Err(anyhow::anyhow!(
                    "failed to load encryption key: {e}. To run with an insecure dev-only key, \
                     set AURA_ALLOW_DEV_ENCRYPTION_KEY=1 (NOT for production — secrets would be \
                     encrypted with a publicly-known key)"
                ));
            }
            error!(
                error = %e,
                "AURA_ALLOW_DEV_ENCRYPTION_KEY=1 — running with dev-only encryption key; \
                 secrets are NOT confidential, DO NOT use in production"
            );
            Ok(EncryptionKey::new(
                b"aura-dev-master-key-32-bytes-ok!".to_vec(),
            )?)
        }
    }
}

/// Open the project's libsql store just far enough to build a
/// [`SecretVault`]. Used by the gateway's vault-only subcommands so
/// they don't have to pay the cost of a full [`build_managers`] call
/// (job recovery, cron scheduler, tool registry, etc.) to read or
/// rotate a token.
pub async fn build_secret_vault(config: &AuraConfig) -> anyhow::Result<Arc<SecretVault>> {
    let storage = Store::open(boot::storage_db_path(&config.workspace)).await?;
    let master_key = load_master_key(&config.security)?;
    Ok(Arc::new(SecretVault::new(master_key, storage.secret)))
}

/// Open the libsql store and return the vault + full [`Store`] handle
/// for CLI subcommands that manage per-channel credentials
/// (`aura channel list/add/remove`) or per-user pairings
/// (`aura pair list/approve/revoke`). The returned [`Store`] is cloneable
/// and its fields share a single libsql connection, so CLI writes land
/// atomically in the same file the gateway reads from.
pub async fn build_bot_registry_deps(
    config: &AuraConfig,
) -> anyhow::Result<(Arc<SecretVault>, Store)> {
    let storage = Store::open(boot::storage_db_path(&config.workspace)).await?;
    let master_key = load_master_key(&config.security)?;
    let vault = Arc::new(SecretVault::new(master_key, storage.secret.clone()));
    Ok((vault, storage))
}

/// Fully-wired manager graph shared between the TUI and gateway boot
/// paths. Every manager is an `Arc` so the caller can clone handles
/// into adapters and background tasks freely.
///
/// The `cron_trigger_rx` is a one-shot — [`wire_router`] consumes it
/// when attaching cron triggers to the router.
pub struct ManagerGraph {
    pub config: Arc<AuraConfig>,
    pub session_manager: Arc<SessionManager>,
    pub job_lifecycle: Arc<JobLifecycle>,
    pub memory_manager: Arc<MemoryManager>,
    pub cron_scheduler: Arc<CronScheduler>,
    pub security_gateway: Arc<SecurityGateway>,
    pub skill_registry: Arc<SkillRegistry>,
    pub tool_registry: Arc<ToolRegistry>,
    pub tool_executor: Arc<ToolExecutor>,
    /// Always already wrapped via `GuardedLlm` — every
    /// consumer (main loop, side-LLM in tools, code_builder,
    /// skill_assessor) shares the same budget gate. Constructed in
    /// [`build_managers`] so a new consumer added downstream can't
    /// accidentally pull a raw `Arc<LlmClient>` and bypass the gate;
    /// the type signature alone is enough to refuse a raw
    /// `Arc<dyn LlmCompletion>` at the call site.
    pub llm_client: Arc<GuardedLlm>,
    /// `llm_client` is `pool.default_client()`. The pool exists so the
    /// actor spawner can resolve a per-session pick from
    /// `Session.state.last_llm`.
    pub llm_pool: Arc<aura_agent::LlmClientPool>,
    /// Owner of cost-record persistence and the budget gate used by
    /// `llm_client` above. Kept on the graph so `wire_router` can
    /// hand it to `AgentLoop` without reconstructing — and so the
    /// gate built into `llm_client` and the records the agent loop
    /// writes are guaranteed to come from the same instance.
    pub cost_manager: Arc<CostManager>,
    pub workspace: Arc<WorkspaceManager>,
    pub channels_registry: Arc<ChannelRegistry>,
    pub secret_vault: Arc<SecretVault>,
    /// Cloneable bundle of every libsql-backed store handle. Keeping the
    /// whole [`Store`] in one field means adding a new store only
    /// touches [`Store`] itself — the graph and its downstream consumers
    /// pick it up via `stores.xxx` without a new field here.
    pub stores: Store,

    /// Consumed by [`wire_router`]. Stored in the graph so the caller
    /// cannot forget to plumb it through — a silently-missing receiver
    /// would make cron-triggered turns disappear into the void. Wrapped
    /// in `Option` so [`wire_router`] can `.take()` it, and calling
    /// `wire_router` twice panics loudly instead of silently handing
    /// out a dummy receiver.
    pub cron_trigger_rx: Option<mpsc::Receiver<CronTriggerEvent>>,

    /// Sender half of the system-spawn channel. The agent loop's
    /// trigger gate sends `SystemSpawnRequest` values here; the
    /// receiving half lives on the router until [`wire_router`]
    /// `.take()`s it. Cloned into every `AgentLoop` the spawner
    /// factory builds so any actor can schedule a maintenance task.
    pub system_spawn_tx: mpsc::Sender<SystemSpawnRequest>,

    /// Receiving half of the system-spawn channel. Same `Option` +
    /// `take`-on-wire pattern as `cron_trigger_rx` — calling
    /// `wire_router` twice panics rather than silently dropping
    /// system-spawn requests.
    pub system_spawn_rx: Option<mpsc::Receiver<SystemSpawnRequest>>,

    /// Process-wide parent token for `AgentActor`s. Bridged to the
    /// shared `ShutdownSignal` in [`build_managers`]; cancelling it
    /// cascades down through every actor's per-job cancel tree.
    pub actor_parent_token: CancellationToken,
}

/// Resolve every domain manager, tying them together with the shared
/// `shutdown` signal. Consumes the storage graph (`Store::open`) in the
/// process — the caller keeps only the per-trait `Arc` handles returned
/// inside [`ManagerGraph`].
pub async fn build_managers(
    config: Arc<AuraConfig>,
    shutdown: ShutdownSignal,
    leak_detector: Arc<LeakDetector>,
    // Pre-assembled embedded MCP server entries the reconciler should
    // spawn alongside any user-configured `.mcp.json` entries. Built
    // by the boot-path caller (`gateway_cmd::start` for the gateway,
    // empty `Vec` for non-gateway paths) so the manager-graph layer
    // stays free of per-tool-domain wiring (browser blob upload, etc).
    embedded_mcp_servers: Vec<EmbeddedMcpServer>,
) -> anyhow::Result<ManagerGraph> {
    // --- minimal services shared by every mode
    let workspace_paths =
        aura_workspace::WorkspacePaths::new(std::path::PathBuf::from(&config.workspace.path));
    let workspace_root = workspace_paths.root().to_path_buf();
    let skill_registry = {
        let reg = Arc::new(SkillRegistry::new());
        let builtins = reg.register_builtins();
        if builtins > 0 {
            info!(count = builtins, "registered built-in skills");
        }
        let workspace_skills = workspace_paths.skills_dir();
        let loaded = reg.load_dir(&workspace_skills);
        if loaded > 0 {
            info!(
                count = loaded,
                path = %workspace_skills.display(),
                "loaded skills from workspace"
            );
        }
        reg
    };
    let workspace = Arc::new(WorkspaceManager::new(workspace_root.clone()));
    let channels_registry = Arc::new(ChannelRegistry::new());
    // Eagerly install every enabled channel from config so connections
    // can attach to a pre-existing slot rather than racing on lazy
    // first-connect creation. See docs/modules/channels-protocol-refactor.md.
    aura_gateway::channel::boot::install_channels(&channels_registry, &config.channels)?;

    // --- storage + domain managers. `stores` is kept whole: every Arc
    // handed to a manager is a cheap `stores.xxx.clone()` so the bundle
    // itself stays intact for the graph + downstream consumers.
    let stores = Store::open(boot::storage_db_path(&config.workspace)).await?;

    // Browser tools are not registered as builtins — they arrive
    // dynamically when the embedded browser MCP server connects via
    // the reconciler below. Until that connect completes (or if the
    // child crashes), the LLM does not see `browser/*` tools at all.
    //
    // Built as a plain mutable up front so the registration steps
    // below (cron, code-builder) can `register` directly. Wrapped in
    // `Arc` once all registrations are done — that way the "single
    // owner" invariant the registrations relied on is enforced by the
    // type system, not by an `Arc::get_mut().expect()` runtime check.
    // Vault is constructed up here (before `build_llm_client`) so the
    // openai-subscription provider can read its OAuth bundle straight away.
    // Other providers ignore the vault. Comment from the original site:
    // can't route through `build_secret_vault` (it would re-open libsql);
    // share the master-key policy via `load_master_key`.
    let master_key = load_master_key(&config.security)?;
    let secret_vault = Arc::new(SecretVault::new(master_key, stores.secret.clone()));

    // CostManager built before the LLM client so its gate closure is
    // ready for `boot::build_llm_client` to seal into `GuardedLlm`.
    // The provider registry is shared between pricing harvest and
    // `build_llm_client` — single source of truth for factories.
    let provider_registry = aura_llm::LlmProviderRegistry::with_default_providers();
    let pricings = provider_registry.all_known_pricings();
    let spending_limits = SpendingLimits {
        daily_usd: config.cost.spending_limits.daily_usd,
        monthly_usd: config.cost.spending_limits.monthly_usd,
    };
    let cost_manager = CostManager::new(stores.cost.clone(), pricings, spending_limits);
    cost_manager.hydrate().await;

    // Refresh pricing for every configured entry (not just default —
    // subagents and side-LLM consumers pin specific names; missing
    // one widens the global budget since `cost_records` is keyed by
    // model id). Spawned so boot doesn't await the network.
    let configured_for_refresh: Vec<(String, String)> = config
        .llm
        .iter()
        .map(|e| (e.provider.clone(), e.model.clone()))
        .collect();
    if !configured_for_refresh.is_empty() {
        let cm_for_refresh = Arc::clone(&cost_manager);
        let refresh_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let entries: Vec<(&str, &str)> = configured_for_refresh
                .iter()
                .map(|(p, m)| (p.as_str(), m.as_str()))
                .collect();
            loop {
                let overlay = aura_llm::openrouter::fetch_overlay_for(&entries).await;
                let pricings = overlay
                    .into_iter()
                    .map(|(model, (pricing, _caps))| (model, pricing))
                    .collect();
                cm_for_refresh.merge_pricings(pricings);
                tokio::select! {
                    _ = tokio::time::sleep(aura_llm::openrouter::REFRESH_INTERVAL) => {}
                    _ = refresh_shutdown.wait() => break,
                }
            }
        });
    }

    // One `Arc<GuardedLlm>` per `cfg.llm[*]` entry, built concurrently.
    // Entries that fail to build are absent from the pool; the default
    // entry failing is a hard error.
    let entry_results = futures::future::join_all(config.llm.iter().map(|entry| {
        let provider_registry = &provider_registry;
        let blob = stores.blob.clone();
        let vault = Arc::clone(&secret_vault);
        let cost_guard = cost_manager.as_guard();
        async move {
            let result = boot::build_llm_client_for_entry(
                entry,
                provider_registry,
                Some(blob),
                Some(vault),
                cost_guard,
            )
            .await;
            (entry.name.clone(), result)
        }
    }))
    .await;
    let mut pool_clients: std::collections::HashMap<String, Arc<aura_llm::GuardedLlm>> =
        std::collections::HashMap::new();
    for (name, result) in entry_results {
        match result {
            Ok(client) => {
                pool_clients.insert(name, client);
            }
            Err(e) => {
                if name == config.default_llm {
                    return Err(e);
                }
                tracing::warn!(
                    entry = %name,
                    error = %e,
                    "failed to build LLM client for entry; the entry is unavailable until the issue is resolved"
                );
            }
        }
    }
    let llm_pool = Arc::new(
        aura_agent::LlmClientPool::new(pool_clients, config.default_llm.clone())
            .map_err(|e| anyhow::anyhow!("build LLM client pool: {e}"))?,
    );
    let llm_client = llm_pool.default_client();
    let info = llm_client.model_info();
    info!(
        provider = %info.provider,
        model = %info.id,
        pool_entries = %llm_pool.entry_names().join(", "),
        supports_vision = info.supports_vision,
        "configured LLM client pool"
    );

    let mut tool_registry = ToolRegistry::with_defaults(
        stores.blob.clone(),
        aura_workspace::WorkspacePaths::new(workspace_root.clone()),
    );

    let assessment_mode = boot::to_assessment_mode(config.skills.risk_check);
    let skill_assessor = Arc::new(SkillAssessor::with_background_worker(
        llm_client.clone(),
        stores.risk.clone(),
        assessment_mode,
    ));
    {
        let registry = Arc::clone(&skill_registry);
        let lookup = move |name: &str| registry.get(name);
        match skill_assessor.recover_pending_jobs(lookup).await {
            Ok(0) => {}
            Ok(n) => info!(count = n, "re-enqueued skill-risk jobs from prior run"),
            Err(e) => tracing::warn!(error = %e, "failed to recover skill-risk jobs"),
        }
    }

    let session_manager = Arc::new(SessionManager::new(
        stores.session.clone(),
        stores.session_summary.clone(),
    ));

    // Reap orphans from any maintenance sessions that were
    // running when the previous process exited. Best-effort —
    // logged at warn on failure, never blocks boot. Runs before
    // any actor spawns so newly-created background-summary actors
    // don't race against stale rows.
    aura_agent::compression::reap_maintenance_orphans(session_manager.as_ref(), &workspace_paths)
        .await;

    let job_lifecycle = Arc::new(JobLifecycle::new(stores.job.clone()));
    // CostTracker has been retired in favour of a process-wide
    // --- cron scheduler (built before ToolExecutor so its tools register
    // while `tool_registry` still has a single Arc owner)
    let (cron_trigger_tx, cron_trigger_rx) = mpsc::channel(64);
    let cron_scheduler = Arc::new(CronScheduler::new(
        stores.cron.clone(),
        cron_trigger_tx,
        Arc::new(shutdown.clone()) as Arc<dyn aura_cron::Shutdown>,
    ));
    // System-spawn channel: agent_loop's trigger gate (sender end) ↔
    // router's `system_trigger_rx` arm (receiver end).
    //
    // Per-parent serialization (`active_maintenance_for_parent` check)
    // caps queue depth at one outstanding request per active parent
    // session, so the upper bound is roughly the number of parents
    // that cross the trigger thresholds in the same instant the
    // router happens to be busy. Each request is ~100–200 B
    // (`SessionId` + `JobId` + `Arc<CancellationToken>` +
    // `BackgroundCompressionPayload`); 1024 slots is ~200 KB and gives a
    // multi-user gateway comfortable headroom over its concurrent
    // active session count, so a `try_send` failure becomes a real
    // backpressure alarm rather than routine bursty drops. Bump
    // further if a deployment regularly trips it.
    let (system_spawn_tx, system_spawn_rx) = mpsc::channel::<SystemSpawnRequest>(1024);
    for (tool, manifest) in aura_cron::agent_tools(Arc::clone(&cron_scheduler)) {
        tool_registry.register(tool, manifest);
    }

    // `spawn_subagent` is just another tool from the LLM's perspective.
    // It ferries the spawn request to the router via the same
    // system-spawn channel that background-compression uses; the router
    // does the session-create + actor-spawn + wait, and ships the
    // final `SubagentResult` back through a oneshot the tool blocks on.
    {
        let (tool, manifest) = aura_tools::builtin::spawn_subagent::make(system_spawn_tx.clone());
        tool_registry.register(tool, manifest);
    }

    // --- Skill tool — registered with the risk assessor as the
    // gate. Lives in aura-skills (parallel to aura-cron::tools)
    // because it needs the registry + assessor; both are constructed
    // above. Always registered: when the registry is empty the
    // per-turn system reminder is suppressed and the LLM never tries
    // the call.
    {
        let risk_check: Arc<dyn aura_skills::SkillRiskCheck> = Arc::clone(&skill_assessor) as _;
        let (tool, manifest) =
            aura_skills::build_skill_tool(Arc::clone(&skill_registry), Arc::clone(&risk_check));
        tool_registry.register(tool, manifest);

        let (install_tool, install_manifest) = aura_skills::tools::build_install_tool(
            workspace_paths.skills_dir(),
            Arc::clone(&skill_registry),
            risk_check,
        );
        tool_registry.register(install_tool, install_manifest);

        let (uninstall_tool, uninstall_manifest) = aura_skills::tools::build_uninstall_tool(
            workspace_paths.skills_dir(),
            Arc::clone(&skill_registry),
        );
        tool_registry.register(uninstall_tool, uninstall_manifest);
    }

    // --- security gateway + tool executor
    let tool_spill_dir =
        aura_workspace::WorkspacePaths::new(workspace_root.clone()).tool_spills_dir();
    let security_gateway = Arc::new(
        SecurityGateway::new(Arc::clone(&leak_detector), Arc::clone(&secret_vault))
            .with_spill_dir(tool_spill_dir),
    );
    let gate_map = channels_registry.approval_gates();
    let sandbox_runner = match aura_sandbox::current_platform_runner() {
        Ok(r) => match r.warm().await {
            Ok(()) => {
                info!(backend = ?r.backend(), "OS sandbox ready");
                Some(r)
            }
            Err(e) => {
                error!(
                    error = %e,
                    backend = ?r.backend(),
                    "sandbox warm-up failed; ExecCommand tools will be refused",
                );
                None
            }
        },
        Err(
            e @ (aura_sandbox::SandboxError::BackendMissing { .. }
            | aura_sandbox::SandboxError::BackendUnreachable { .. }
            | aura_sandbox::SandboxError::NoBackendAvailable),
        ) => {
            error!(error = %e, "OS sandbox unavailable; ExecCommand tools will be refused");
            None
        }
        Err(e) => {
            return Err(e.into());
        }
    };

    // --- code-builder tool: needs the sandbox runner (to execute
    // generated code under per-call caps) and the leak detector +
    // vault so revealed tool args can be re-sanitized before they
    // reach the nested planning LLM. The planner LLM is NOT captured
    // here — CodeBuilder reads `ctx.llm` (per-call billed handle
    // bound to the surrounding actor's current LLM) so a session
    // pinned to a non-default model cascades into the planner. Skip
    // registration if the sandbox is unavailable — CodeBuilder would
    // refuse every call without it.
    if let Some(runner) = sandbox_runner.as_ref() {
        let (tool, manifest) = aura_code_builder::agent_tool(
            Arc::clone(runner),
            Arc::clone(&leak_detector),
            Arc::clone(&secret_vault),
        );
        tool_registry.register(tool, manifest);
    } else {
        tracing::warn!("CodeBuilder tool not registered: OS sandbox unavailable");
    }

    // Freeze the registry now that mutation is done; downstream
    // consumers (`tool_executor`, `McpReconciler`, the actor spawner)
    // need an `Arc<ToolRegistry>` for sharing across tasks.
    let tool_registry = Arc::new(tool_registry);

    // Sandbox FS scope is the workspace `work/` directory — the
    // ephemeral scratch root for tool-generated files. `ensure_layout`
    // creates this before `build_managers` runs. Canonicalize so
    // symlink-vs-real-path comparisons in the adapter line up with
    // paths the tool may produce.
    let work_dir = workspace_paths.work_dir();
    let sandbox_root = work_dir.canonicalize().unwrap_or_else(|e| {
        tracing::warn!(
            error = %e,
            path = %work_dir.display(),
            "failed to canonicalize sandbox work dir; using literal path",
        );
        work_dir
    });
    info!(path = %sandbox_root.display(), "sandbox FS scope rooted at workspace work/");
    // The per-actor `BilledChatFactory` now lives on each
    // `AgentLoop` (constructed by the spawner once the actor's
    // chosen LLM is resolved), so `ToolExecutor` no longer stores
    // one — it's passed in per `execute` call.
    let tool_executor = Arc::new(ToolExecutor::new(
        Arc::clone(&tool_registry),
        gate_map,
        Arc::clone(&security_gateway),
        sandbox_root,
        workspace_paths.clone(),
        sandbox_runner,
    ));

    let memory_manager = Arc::new(MemoryManager::without_embedder(stores.memory.clone()));

    // --- MCP reconciler — re-reads <workspace>/.mcp.json every 5s and
    // dynamically registers/unregisters MCP-discovered tools. Bridge the
    // shared `ShutdownSignal` to a `CancellationToken` since the
    // reconciler lives in `aura-tools`, which doesn't depend on
    // `aura-agent`.
    let mcp_cancel = CancellationToken::new();
    {
        let signal = shutdown.clone();
        let cancel_on_shutdown = mcp_cancel.clone();
        tokio::spawn(async move {
            signal.wait().await;
            cancel_on_shutdown.cancel();
        });
    }

    // --- per-actor parent token. The spawner factory derives each
    // actor's `actor_token` as a child of this; tripping it on
    // shutdown cascades cancel through every in-flight tool /
    // subagent across every session.
    let actor_parent_token = CancellationToken::new();
    {
        let signal = shutdown.clone();
        let cancel_on_shutdown = actor_parent_token.clone();
        tokio::spawn(async move {
            signal.wait().await;
            cancel_on_shutdown.cancel();
        });
    }
    // Browser MCP server: shipped as a zstd-embedded JS bundle, run
    // by the gateway as a stdio MCP child. The reconciler spawns it
    // alongside any user-configured `.mcp.json` entries; if the bundle
    // failed to materialise (`SidecarRuntime::install` Err), the
    // embedded list is empty and only user entries get connected.
    let mcp_reconciler = McpReconciler::new(
        workspace_root.clone(),
        Arc::clone(&tool_registry),
        Arc::clone(&secret_vault),
        Some(stores.blob.clone()),
        embedded_mcp_servers,
        mcp_cancel,
    );
    mcp_reconciler.spawn();

    Ok(ManagerGraph {
        config,
        session_manager,
        job_lifecycle,
        memory_manager,
        cron_scheduler,
        security_gateway,
        skill_registry,
        tool_registry,
        tool_executor,
        llm_client,
        llm_pool,
        cost_manager,
        workspace,
        channels_registry,
        secret_vault,
        stores,
        cron_trigger_rx: Some(cron_trigger_rx),
        system_spawn_tx,
        system_spawn_rx: Some(system_spawn_rx),
        actor_parent_token,
    })
}

/// Router + channel handles a chat loop needs to drive.
///
/// `incoming_tx` is handed to each channel transport at registration
/// time (the WS sidecar pulls it via `GatewayDeps`); `incoming_rx`
/// and `response_rx` feed [`Router::run`]. Dropping the handle before
/// calling `.run` leaks the
/// router's background actor spawner, so callers should either drive it
/// or drop the whole graph together.
pub struct RouterRunHandle {
    pub router: Router,
    pub incoming_tx: mpsc::Sender<IncomingMessage>,
    pub incoming_rx: mpsc::Receiver<IncomingMessage>,
    pub response_rx: mpsc::Receiver<AgentOutput>,
}

/// Build the [`Router`] and wire a per-session actor spawner against
/// the graph. The returned handle already has cron triggers attached —
/// every cron-enqueued session reaches the router regardless of entry
/// point.
pub async fn wire_router(graph: &mut ManagerGraph) -> RouterRunHandle {
    let buffer = graph.config.channels.message_buffer_size;

    let session_log_dir =
        aura_workspace::WorkspacePaths::new(std::path::PathBuf::from(&graph.config.workspace.path))
            .sessions_log_dir();
    let session_logger = Arc::new(SessionLlmLogger::new(session_log_dir));

    let tokenizer: Arc<dyn Tokenizer> = Arc::new(TiktokenTokenizer::for_model(
        graph.llm_client.model_info().id.as_str(),
    ));

    let token_calibration = Arc::new(aura_context::TokenCalibration::new());

    let soul = Soul::from_workspace(&graph.workspace)
        .await
        .unwrap_or_else(|_| Soul::custom("You are Aura, an intelligent assistant.".to_string()));
    let system_prompt = soul.raw_template().to_string();

    let policy = boot::to_execution_policy(&graph.config.agent);
    let token_budget = boot::to_token_budget(&graph.config.agent.context);
    let keep_recent = graph.config.agent.context.keep_recent;

    let (incoming_tx, incoming_rx) = mpsc::channel(buffer);
    let (response_tx, response_rx) = mpsc::channel(buffer);

    let supervisor = AgentSupervisor::new(response_tx);

    // Process-wide trace event bus. Stays for trace observers
    // (WebUI live stream etc.); `CostManager` no longer subscribes
    // — agent_loop calls it directly with the token counts it
    // already has.
    let trace_event_stream = TraceEventStream::new();

    // `cost_manager` and the guarded `llm_client` are both built in
    // `build_managers` and live on `graph` — every consumer (main
    // loop, side-LLM in tools, code_builder, skill_assessor) shares
    // the same gate. Re-binding here just to keep the local name
    // changes below minimal.
    let cost_manager = Arc::clone(&graph.cost_manager);
    let llm_pool = Arc::clone(&graph.llm_pool);

    // Single boxed factory owned by the router: used for top-level
    // user/cron actors, background-compression maintenance spawns,
    // AND `SystemSpawnRequest::Subagent` child materialisation.
    let spawn_actor_for: aura_agent::router::ActorSpawner = {
        let llm_pool = Arc::clone(&llm_pool);
        let tool_registry = Arc::clone(&graph.tool_registry);
        let skill_registry = Arc::clone(&graph.skill_registry);
        let tool_executor = Arc::clone(&graph.tool_executor);
        let memory_manager = Arc::clone(&graph.memory_manager);
        let trace_store = graph.stores.trace.clone();
        let job_lifecycle = Arc::clone(&graph.job_lifecycle);
        let security_gateway = Arc::clone(&graph.security_gateway);
        let session_logger = Arc::clone(&session_logger);
        let tokenizer = Arc::clone(&tokenizer);
        let trace_event_stream = trace_event_stream.clone();
        let token_budget = token_budget.clone();
        let policy = policy.clone();
        let system_prompt = system_prompt.clone();
        let cost_manager = Arc::clone(&cost_manager);
        let token_calibration = Arc::clone(&token_calibration);

        let sessions = Arc::clone(&graph.session_manager);
        let workspace_paths_arc = Arc::new(aura_workspace::WorkspacePaths::new(
            graph.workspace.root.clone(),
        ));
        let system_spawn_tx = graph.system_spawn_tx.clone();
        let supervisor_for_spawn = supervisor.clone();
        Box::new(
            move |session: aura_model::Session,
                  initial_llm: Option<String>,
                  response_tx: mpsc::Sender<AgentOutput>,
                  parent_token: &CancellationToken| {
                // Derive the actor's lifetime token here, threaded
                // into both the loop (so its summary trigger gate can
                // clone it into outgoing SystemSpawnRequests) and the
                // actor itself.
                let actor_token = parent_token.child_token();

                // LLM pinning is exclusively a subagent-spawn affair —
                // user-channel actors always run on `default-llm`.
                // `initial_llm` is `Some` only when the router's
                // `handle_subagent_spawn` forwards a
                // `SubagentSpawnRequest.llm`.
                let effective_initial = initial_llm;

                // `summary_state_dir` connects the compressor's
                // fast-path to the background refresh runner's output.
                // Without it the background passes still run and bill
                // LLM, but their summaries never reach the hot path.
                // See `docs/background-compression.md`.
                let agent_loop = AgentLoop::from_config(AgentLoopConfig {
                    llm_pool: Arc::clone(&llm_pool),
                    initial_llm: effective_initial,
                    tool_registry: Arc::clone(&tool_registry),
                    skill_registry: Arc::clone(&skill_registry),
                    tool_executor: Arc::clone(&tool_executor),
                    context_manager: ContextManager::from_config(ContextManagerConfig {
                        tokenizer: Arc::clone(&tokenizer),
                        workspace: Arc::clone(&workspace_paths_arc),
                        keep_recent,
                        budget: token_budget.clone(),
                        calibration: Arc::clone(&token_calibration),
                        skill_registry: Arc::clone(&skill_registry),
                        session_id: session.id.clone(),
                        sessions: Arc::clone(&sessions),
                    }),
                    memory_manager: Arc::clone(&memory_manager),
                    policy: policy.clone(),
                    soul: Soul::custom(system_prompt.clone()),
                    security_gateway: Arc::clone(&security_gateway),
                    cost_manager: Arc::clone(&cost_manager),
                    actor_token: actor_token.clone(),
                    session_log: Some(Arc::clone(&session_logger)),
                    system_spawn_tx: Some(system_spawn_tx.clone()),
                    workspace_paths: Some(Arc::clone(&workspace_paths_arc)),
                    sessions: Some(Arc::clone(&sessions)),
                });

                let span_recorder = Arc::new(SpanRecorder::new(
                    session.id.clone(),
                    session.user.id.clone(),
                    Arc::clone(&trace_store),
                    trace_event_stream.clone(),
                ));

                let actor = AgentActor::from_parts(
                    aura_agent::state::DurableActorState::new(session),
                    aura_agent::state::VolatileResources {
                        agent_loop,
                        response_tx,
                        job_lifecycle: Arc::clone(&job_lifecycle),
                        span_recorder,
                        actor_token,
                        supervisor: Some(supervisor_for_spawn.clone()),
                    },
                );
                let (sender, mailbox) = mpsc::channel(buffer);
                tokio::spawn(async move {
                    actor.run(mailbox).await;
                });
                sender
            },
        )
    };

    let rate_limit_cfg = &graph.config.cost.rate_limit;

    // `take` cron + system rxs eagerly — a caller who forgot to plumb
    // either would silently drop every cron-fired turn / maintenance
    // trigger. Calling `wire_router` twice panics here loudly rather
    // than silently handing out a dummy receiver.
    let cron_trigger_rx = graph
        .cron_trigger_rx
        .take()
        .expect("wire_router called twice; cron_trigger_rx already consumed");
    let system_trigger_rx = graph
        .system_spawn_rx
        .take()
        .expect("wire_router called twice; system_spawn_rx already consumed");

    // Idle actor reaper: shuts down registered actors whose sessions
    // have been idle. Hydration on the next user message rebuilds the
    // actor from the durable session row, so eviction is lossless for
    // an actor with no in-flight turn. The session row itself is never
    // deleted.
    aura_agent::supervisor::spawn_idle_reaper(
        supervisor.clone(),
        Arc::clone(&graph.session_manager),
        graph.actor_parent_token.clone(),
    );

    let router = Router::from_config(aura_agent::router::RouterConfig {
        session_manager: Arc::clone(&graph.session_manager),
        supervisor,
        channels: Arc::clone(&graph.channels_registry),
        security_gateway: Arc::clone(&graph.security_gateway),
        cost_manager: Arc::clone(&cost_manager),
        actor_spawner: spawn_actor_for,
        job_lifecycle: Arc::clone(&graph.job_lifecycle),
        cron_trigger_rx,
        system_trigger_rx,
        actor_parent_token: graph.actor_parent_token.clone(),
        rate_limit_max_requests: rate_limit_cfg.max_requests,
        rate_limit_window: std::time::Duration::from_secs(rate_limit_cfg.window_secs),
    });

    RouterRunHandle {
        router,
        incoming_tx,
        incoming_rx,
        response_rx,
    }
}

/// Wire SIGINT / SIGTERM (Unix) or Ctrl-C (non-Unix) into the provided
/// [`ShutdownSignal`]. Tracked on the supplied [`TaskTracker`] so the
/// handler gets awaited during graceful teardown.
pub fn install_signal_handler(tracker: &mut TaskTracker, shutdown: ShutdownSignal) {
    tracker.track(tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM");
            tokio::select! {
                _ = ctrl_c => { info!("received SIGINT"); }
                _ = sigterm.recv() => { info!("received SIGTERM"); }
            }
        }
        #[cfg(not(unix))]
        {
            ctrl_c.await.expect("failed to listen for ctrl_c");
            info!("received SIGINT");
        }
        shutdown.trigger();
    }));
}

/// Start an OS thread that force-exits the process if the runtime fails
/// to unwind within `budget`. A blocking tool call that won't yield on
/// shutdown can otherwise stall the tokio drop indefinitely.
pub fn force_exit_watchdog(budget: std::time::Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(budget);
        eprintln!(
            "aura: graceful shutdown exceeded {}s, force-exiting",
            budget.as_secs()
        );
        std::process::exit(0);
    });
}
