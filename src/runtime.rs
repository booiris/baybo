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
use aura_agent::agent_loop::AgentLoop;
use aura_agent::router::Router;
use aura_agent::service::{ShutdownSignal, TaskTracker};
use aura_agent::session_log::SessionLlmLogger;
use aura_agent::soul::Soul;
use aura_agent::subagent::{LocalSubagentRuntime, SubagentRuntime};
use aura_agent::supervisor::AgentSupervisor;
use aura_agent::tool_executor::ToolExecutor;
use aura_agent::{
    CronScheduler, CronTriggerEvent, JobLifecycle, MemoryManager, SecretVault, SecurityGateway,
    SessionManager, SpanRecorder, TraceEventStream,
};
use aura_channels::{AgentOutput, ChannelRegistry, IncomingMessage};
use aura_config::AuraConfig;
use aura_context::{ContextManager, TiktokenTokenizer, Tokenizer, Truncate};
use aura_llm::LlmClient;
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
    pub skill_assessor: Arc<SkillAssessor>,
    pub tool_registry: Arc<ToolRegistry>,
    pub tool_executor: Arc<ToolExecutor>,
    pub llm_client: Arc<LlmClient>,
    pub workspace: Arc<WorkspaceManager>,
    pub channels_registry: Arc<ChannelRegistry>,
    /// `CostStore` retained on the graph so external subscribers can
    /// open their own `TraceEventStream` listeners (e.g. the gateway).
    pub cost_store: Arc<dyn aura_storage::CostStore>,
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

    // Built before the tool registry so `default_tools` can wire the
    // side-LLM into `WebFetch` for prompt-driven extraction. Also
    // built after `stores` so `build_llm_client` can wire the blob
    // store in as a `BlobFetcher` — without it, multimodal user
    // content would degrade to a `[image: …]` text stub even on
    // vision-capable models.
    let llm_client = {
        let client = Arc::new(
            boot::build_llm_client(
                config.as_ref(),
                Some(stores.blob.clone()),
                Some(Arc::clone(&secret_vault)),
            )
            .await?,
        );
        info!(
            provider = %client.model_info().provider,
            model = %client.model_id(),
            supports_vision = client.model_info().supports_vision,
            "configured LLM client"
        );
        client
    };

    let mut tool_registry = ToolRegistry::with_defaults(
        stores.blob.clone(),
        Some(Arc::clone(&llm_client) as Arc<dyn aura_llm::LlmCompletion>),
    );

    let assessment_mode = boot::to_assessment_mode(config.skills.risk_check);
    let skill_assessor = Arc::new(SkillAssessor::with_background_worker(
        Arc::clone(&llm_client),
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
        boot::to_session_timeout(&config.session),
    ));

    let job_lifecycle = Arc::new(JobLifecycle::new(stores.job.clone()));
    // CostTracker has been retired in favour of a process-wide
    // `TraceEventStream` subscriber wired once in `wire_router`
    // against the shared stream that every per-actor `SpanRecorder`
    // publishes into.
    let cost_store_for_subscriber = stores.cost.clone();

    // --- cron scheduler (built before ToolExecutor so its tools register
    // while `tool_registry` still has a single Arc owner)
    let (cron_trigger_tx, cron_trigger_rx) = mpsc::channel(64);
    let cron_scheduler = Arc::new(CronScheduler::new(
        stores.cron.clone(),
        cron_trigger_tx,
        Arc::new(shutdown.clone()) as Arc<dyn aura_cron::Shutdown>,
    ));
    for (tool, manifest) in aura_cron::agent_tools(Arc::clone(&cron_scheduler)) {
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

    // --- code-builder tool: needs the LLM client (for codegen), the
    // sandbox runner (to execute generated code under per-call caps),
    // and the leak detector + vault so revealed tool args can be
    // re-sanitized before they reach the nested planning LLM. Skip
    // registration if the sandbox is unavailable — CodeBuilder would
    // refuse every call without it.
    if let Some(runner) = sandbox_runner.as_ref() {
        let llm: Arc<dyn aura_llm::LlmCompletion> = Arc::clone(&llm_client) as _;
        let (tool, manifest) = aura_code_builder::agent_tool(
            llm,
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

    // --- per-actor parent token. Each `AgentActor::new` derives a child
    // from this; tripping it on shutdown cascades cancel through every
    // in-flight tool / subagent across every session.
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
        skill_assessor,
        tool_registry,
        tool_executor,
        llm_client,
        workspace,
        channels_registry,
        cost_store: cost_store_for_subscriber,
        secret_vault,
        stores,
        cron_trigger_rx: Some(cron_trigger_rx),
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

    let workspace_paths =
        aura_workspace::WorkspacePaths::new(std::path::PathBuf::from(&graph.config.workspace.path));
    let session_log_dir = workspace_paths.sessions_log_dir();
    let session_logger = Arc::new(SessionLlmLogger::new(session_log_dir));

    let tokenizer: Arc<dyn Tokenizer> =
        Arc::new(TiktokenTokenizer::for_model(graph.llm_client.model_id()));

    let soul = Soul::from_workspace(&graph.workspace)
        .await
        .unwrap_or_else(|_| Soul::custom("You are Aura, an intelligent assistant.".to_string()));
    let system_prompt = soul.system_prompt().to_string();

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
    let pricing: Arc<std::collections::HashMap<String, aura_llm::ModelPricing>> = {
        // Seed with every provider's `known_pricings()`, then layer the
        // active model's pricing on top so a config-flip mid-flight
        // doesn't drop spans of the previous model to $0.
        //
        // Per-model accuracy caveat: today's publishers report one
        // flat rate per provider, so e.g. gpt-5-mini attributes at the
        // same rate as gpt-5. Tracked as a follow-up in
        // `docs/todo/trace-redesign.md`.
        let registry = aura_llm::LlmProviderRegistry::with_default_providers();
        let mut map = registry.all_known_pricings();
        let info = graph.llm_client.model_info();
        map.insert(info.id.clone(), info.pricing);
        Arc::new(map)
    };

    let spending_limits = aura_agent::SpendingLimits {
        daily_usd: graph.config.cost.spending_limits.daily_usd,
        monthly_usd: graph.config.cost.spending_limits.monthly_usd,
    };
    let cost_manager =
        aura_agent::CostManager::new(graph.cost_store.clone(), pricing, spending_limits);
    cost_manager.hydrate().await;

    // Spawner-vs-runtime build cycle: closure captures the slot and is
    // built first, then `LocalSubagentRuntime` patches itself in.
    let subagent_runtime_slot: Arc<std::sync::OnceLock<Arc<dyn SubagentRuntime>>> =
        Arc::new(std::sync::OnceLock::new());

    // Single Arc-shared factory: router's `ActorSpawner` and
    // `LocalSubagentRuntime` both spawn the same fully-wired actor.
    let spawn_actor_for: aura_agent::subagent::SubagentActorSpawner = {
        let llm_client: Arc<dyn aura_llm::LlmCompletion> = Arc::clone(&graph.llm_client) as _;
        let tool_registry = Arc::clone(&graph.tool_registry);
        let skill_registry = Arc::clone(&graph.skill_registry);
        let tool_executor = Arc::clone(&graph.tool_executor);
        let memory_manager = Arc::clone(&graph.memory_manager);
        let trace_store = graph.stores.trace.clone();
        let job_lifecycle = Arc::clone(&graph.job_lifecycle);
        let skill_assessor = Arc::clone(&graph.skill_assessor);
        let security_gateway = Arc::clone(&graph.security_gateway);
        let session_logger = Arc::clone(&session_logger);
        let tokenizer = Arc::clone(&tokenizer);
        let trace_event_stream = trace_event_stream.clone();
        let subagent_runtime_slot = Arc::clone(&subagent_runtime_slot);
        let token_budget = token_budget.clone();
        let policy = policy.clone();
        let system_prompt = system_prompt.clone();
        let cost_manager = Arc::clone(&cost_manager);

        Arc::new(
            move |session: aura_model::Session,
                  response_tx: mpsc::Sender<AgentOutput>,
                  parent_token: &CancellationToken| {
                let mut agent_loop = AgentLoop::new(
                    Arc::clone(&llm_client),
                    Arc::clone(&tool_registry),
                    Arc::clone(&skill_registry),
                    Arc::clone(&tool_executor),
                    ContextManager::new(
                        Arc::clone(&tokenizer),
                        Box::new(Truncate::new(keep_recent)),
                        token_budget.clone(),
                    ),
                    Arc::clone(&memory_manager),
                    policy.clone(),
                    Soul::custom(system_prompt.clone()),
                    Arc::clone(&security_gateway),
                )
                .with_skill_assessor(Arc::clone(&skill_assessor))
                .with_session_log(Arc::clone(&session_logger))
                .with_cost_manager(Arc::clone(&cost_manager));

                if let Some(rt) = subagent_runtime_slot.get() {
                    agent_loop = agent_loop.with_subagent_runtime(Arc::clone(rt));
                }

                let span_recorder = Arc::new(SpanRecorder::new(
                    session.id.clone(),
                    session.user.id.clone(),
                    Arc::clone(&trace_store),
                    trace_event_stream.clone(),
                ));

                let actor = AgentActor::new(
                    session,
                    agent_loop,
                    response_tx,
                    Arc::clone(&job_lifecycle),
                    span_recorder,
                    parent_token,
                );
                let (sender, mailbox) = mpsc::channel(buffer);
                tokio::spawn(async move {
                    actor.run(mailbox).await;
                });
                sender
            },
        )
    };

    let local_subagent_runtime: Arc<dyn SubagentRuntime> = Arc::new(LocalSubagentRuntime::new(
        Arc::clone(&graph.session_manager),
        Arc::clone(&spawn_actor_for),
        Arc::clone(&graph.job_lifecycle),
    ));
    // Sole `set` site in the process — `is_err()` is unreachable barring
    // a programming error in this file.
    let set_ok = subagent_runtime_slot.set(local_subagent_runtime).is_ok();
    debug_assert!(set_ok);

    let rate_limit_cfg = &graph.config.cost.rate_limit;

    let router = Router::new(
        Arc::clone(&graph.session_manager),
        supervisor,
        Arc::clone(&graph.channels_registry),
        Arc::clone(&graph.security_gateway),
    )
    .with_actor_parent_token(graph.actor_parent_token.clone())
    .with_cost_manager(Arc::clone(&cost_manager))
    .with_rate_limit(
        rate_limit_cfg.max_requests,
        std::time::Duration::from_secs(rate_limit_cfg.window_secs),
    )
    .with_actor_spawner(Box::new(move |session, response_tx, parent_token| {
        spawn_actor_for(session, response_tx, parent_token)
    }));

    // Attach cron triggers eagerly — a caller who forgot to plumb the
    // receiver would silently drop every cron-fired turn.
    let cron_trigger_rx = graph
        .cron_trigger_rx
        .take()
        .expect("wire_router called twice; cron_trigger_rx already consumed");
    let router = router.with_cron_triggers(cron_trigger_rx);

    // ── SelfImprovement wiring ──────────────────────────────────────────
    //
    // The self_improvement flow runs in its own AgentLoop with a *separate*
    // tool registry containing only the four `self_improvement_tools`.
    // Registration isolation is the entire protection model for
    // `MemoryWrite` / `SkillCreate` / `MemoryList` / `SkillList`
    // (`docs/modules/self-improvement.md` Q7) — they intentionally bypass
    // the approval gate, which is safe only because they're never
    // exposed to a user-facing actor.
    let router = {
        let dcfg = &graph.config.agent.self_improvement;
        if !dcfg.enabled {
            info!("self_improvement disabled by config; skipping wiring");
            router
        } else {
            // SelfImprovement-specific tool registry + executor. Own
            // `ApprovalGateMap` (empty) is fine — the four tools
            // declare empty `accessed_resources()` so the gate is
            // never consulted.
            let mut dist_registry = aura_tools::ToolRegistry::new();
            for (tool, manifest) in aura_agent::self_improvement::self_improvement_tools(
                Arc::clone(&graph.memory_manager),
                Arc::clone(&graph.skill_registry),
            ) {
                dist_registry.register(tool, manifest);
            }
            let dist_registry = Arc::new(dist_registry);
            let dist_gate_map = Arc::new(aura_tools::ApprovalGateMap::new());
            let dist_executor = Arc::new(aura_agent::ToolExecutor::new(
                Arc::clone(&dist_registry),
                dist_gate_map,
                Arc::clone(&graph.security_gateway),
                workspace_paths.work_dir(),
                workspace_paths.clone(),
                None,
            ));

            // SelfImprovement actor spawner. Mirrors `spawn_actor_for`
            // but swaps the tool registry/executor to the self_improvement
            // pair. Other deps stay the same.
            let dist_llm: Arc<dyn aura_llm::LlmCompletion> = Arc::clone(&graph.llm_client) as _;
            let dist_skill_registry = Arc::clone(&graph.skill_registry);
            let dist_memory_manager = Arc::clone(&graph.memory_manager);
            let dist_trace_store = graph.stores.trace.clone();
            let dist_job_lifecycle = Arc::clone(&graph.job_lifecycle);
            let dist_skill_assessor = Arc::clone(&graph.skill_assessor);
            let dist_security_gateway = Arc::clone(&graph.security_gateway);
            let dist_session_logger = Arc::clone(&session_logger);
            let dist_tokenizer = Arc::clone(&tokenizer);
            let dist_trace_event_stream = trace_event_stream.clone();
            let dist_token_budget = token_budget.clone();
            let dist_policy = policy.clone();
            let dist_system_prompt = system_prompt.clone();
            let dist_cost_manager = Arc::clone(&cost_manager);

            let self_improvement_spawner: aura_agent::ActorSpawner = Box::new(
                move |session: aura_model::Session,
                      response_tx: mpsc::Sender<AgentOutput>,
                      parent_token: &CancellationToken| {
                    let agent_loop = AgentLoop::new(
                        Arc::clone(&dist_llm),
                        Arc::clone(&dist_registry),
                        Arc::clone(&dist_skill_registry),
                        Arc::clone(&dist_executor),
                        ContextManager::new(
                            Arc::clone(&dist_tokenizer),
                            Box::new(Truncate::new(keep_recent)),
                            dist_token_budget.clone(),
                        ),
                        Arc::clone(&dist_memory_manager),
                        dist_policy.clone(),
                        Soul::custom(dist_system_prompt.clone()),
                        Arc::clone(&dist_security_gateway),
                    )
                    .with_skill_assessor(Arc::clone(&dist_skill_assessor))
                    .with_session_log(Arc::clone(&dist_session_logger))
                    .with_cost_manager(Arc::clone(&dist_cost_manager));

                    let span_recorder = Arc::new(SpanRecorder::new(
                        session.id.clone(),
                        session.user.id.clone(),
                        Arc::clone(&dist_trace_store),
                        dist_trace_event_stream.clone(),
                    ));

                    let actor = AgentActor::new(
                        session,
                        agent_loop,
                        response_tx,
                        Arc::clone(&dist_job_lifecycle),
                        span_recorder,
                        parent_token,
                    );
                    let (sender, mailbox) = mpsc::channel(buffer);
                    tokio::spawn(async move {
                        actor.run(mailbox).await;
                    });
                    sender
                },
            );

            // System trigger mpsc + SelfImprovementManager.
            let (system_tx, system_rx) = mpsc::channel(64);
            let dist_config = aura_agent::self_improvement::SelfImprovementConfig {
                enabled: dcfg.enabled,
                min_iterations: dcfg.min_iterations,
                daily_cap: dcfg.daily_cap,
                max_concurrent: dcfg.max_concurrent,
            };
            let manager = Arc::new(aura_agent::self_improvement::SelfImprovementManager::new(
                dist_config,
                Arc::clone(&graph.job_lifecycle),
                graph.stores.session.clone(),
                workspace_paths.clone(),
                system_tx,
            ));
            let _handle = manager.spawn();

            router
                .with_self_improvement_spawner(self_improvement_spawner)
                .with_job_lifecycle(Arc::clone(&graph.job_lifecycle))
                .with_system_triggers(system_rx)
        }
    };

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
