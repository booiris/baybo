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
use aura_agent::cost::CostTracker;
use aura_agent::observability::ObservabilityRecorder;
use aura_agent::router::Router;
use aura_agent::service::{ShutdownSignal, TaskTracker};
use aura_agent::soul::Soul;
use aura_agent::supervisor::AgentSupervisor;
use aura_agent::tool_executor::ToolExecutor;
use aura_agent::{
    CronScheduler, CronTriggerEvent, JobManager, MemoryManager, SecretVault, SecurityGateway,
    SessionManager, TraceCollector,
};
use aura_channels::{AgentOutput, ChannelRegistry, IncomingMessage};
use aura_config::AuraConfig;
use aura_context::{ContextManager, TiktokenTokenizer, Tokenizer, Truncate};
use aura_hook::HookManager;
use aura_llm::LlmClient;
use aura_security::{EncryptionKey, LeakDetectionRule, LeakDetector};
use aura_skills::SkillRegistry;
use aura_skills_assessor::SkillAssessor;
use aura_storage::Store;
use aura_tools::ToolRegistry;
use aura_tools::mcp::McpReconciler;
use aura_workspace::WorkspaceManager;
use parking_lot::Mutex;
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
    pub job_manager: Arc<JobManager>,
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
    pub cost_tracker: Arc<CostTracker>,
    pub hook_manager: Arc<HookManager>,
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
}

/// Resolve every domain manager, tying them together with the shared
/// `shutdown` signal. Consumes the storage graph (`Store::open`) in the
/// process — the caller keeps only the per-trait `Arc` handles returned
/// inside [`ManagerGraph`].
pub async fn build_managers(
    config: Arc<AuraConfig>,
    shutdown: ShutdownSignal,
    leak_detector: Arc<LeakDetector>,
) -> anyhow::Result<ManagerGraph> {
    // --- minimal services shared by every mode
    let workspace_root = std::path::PathBuf::from(&config.workspace.path);
    let skill_registry = {
        let reg = Arc::new(SkillRegistry::new());
        let workspace_skills = workspace_root.join("skills");
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
    let mut tool_registry = Arc::new(ToolRegistry::with_defaults());
    let workspace = Arc::new(WorkspaceManager::new(workspace_root.clone()));
    let channels_registry = Arc::new(ChannelRegistry::new());

    let llm_client = {
        let client = boot::build_llm_client(&config.llm)?;
        let client = Arc::new(client);
        info!(
            provider = %client.model_info().provider,
            model = %client.model_id(),
            "configured LLM client"
        );
        client
    };

    // --- storage + domain managers. `stores` is kept whole: every Arc
    // handed to a manager is a cheap `stores.xxx.clone()` so the bundle
    // itself stays intact for the graph + downstream consumers.
    let stores = Store::open(boot::storage_db_path(&config.workspace)).await?;

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

    let job_manager = JobManager::new(stores.job.clone());
    match job_manager.recover_interrupted().await {
        Ok(0) => {}
        Ok(n) => info!(count = n, "recovered interrupted jobs from prior run"),
        Err(e) => tracing::warn!(error = %e, "failed to recover interrupted jobs"),
    }
    let job_manager = Arc::new(job_manager);

    let cost_tracker = Arc::new(CostTracker::new(stores.cost.clone()));

    // --- secret vault (master key optionally substituted with a dev key).
    // The store is already open here, so we can't route through
    // `build_secret_vault` (it would re-open libsql); share the
    // master-key policy via `load_master_key`.
    let master_key = load_master_key(&config.security)?;
    let secret_vault = Arc::new(SecretVault::new(master_key, stores.secret.clone()));

    // --- cron scheduler (built before ToolExecutor so its tools register
    // while `tool_registry` still has a single Arc owner)
    let (cron_trigger_tx, cron_trigger_rx) = mpsc::channel(64);
    let cron_scheduler = Arc::new(CronScheduler::new(
        stores.cron.clone(),
        cron_trigger_tx,
        Arc::new(shutdown.clone()) as Arc<dyn aura_cron::Shutdown>,
    ));
    {
        let reg = Arc::get_mut(&mut tool_registry)
            .expect("tool_registry has no other owners at this point");
        for (tool, manifest) in aura_cron::agent_tools(Arc::clone(&cron_scheduler)) {
            reg.register(tool, manifest);
        }
    }

    // --- security gateway + tool executor
    let security_gateway = Arc::new(SecurityGateway::new(
        Arc::clone(&leak_detector),
        Arc::clone(&secret_vault),
    ));
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
    // Sandbox FS scope is the *project / cwd*, not Aura's state directory.
    // `workspace_root` above is `config.workspace.path` (`~/.aura`), which is
    // where Aura keeps its libsql + identity files. Bash and other
    // ExecCommand tools should run scoped to where the user launched aura
    // from. Canonicalize so symlink-vs-real-path comparisons in the adapter
    // line up with paths the tool may produce.
    let sandbox_root = match std::env::current_dir().and_then(|p| p.canonicalize()) {
        Ok(cwd) => cwd,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to resolve current_dir for sandbox FS scope; falling back to workspace state directory",
            );
            workspace_root.clone()
        }
    };
    info!(path = %sandbox_root.display(), "sandbox FS scope rooted");
    let tool_executor = Arc::new(ToolExecutor::new(
        Arc::clone(&tool_registry),
        boot::to_tool_timeout(&config.tools),
        gate_map,
        Arc::clone(&security_gateway),
        sandbox_root,
        sandbox_runner,
    ));

    let memory_manager = Arc::new(MemoryManager::without_embedder(stores.memory.clone()));
    let hook_manager = Arc::new(HookManager::new());

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
    let mcp_reconciler = McpReconciler::new(
        workspace_root.clone(),
        Arc::clone(&tool_registry),
        Arc::clone(&secret_vault),
        mcp_cancel,
    );
    mcp_reconciler.spawn();

    Ok(ManagerGraph {
        config,
        session_manager,
        job_manager,
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
        cost_tracker,
        hook_manager,
        secret_vault,
        stores,
        cron_trigger_rx: Some(cron_trigger_rx),
    })
}

/// Router + channel handles a chat loop needs to drive.
///
/// `incoming_tx` is handed to each channel transport at registration
/// time (the WS sidecar pulls it via `ChannelServerDeps`); `incoming_rx`
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

    let tokenizer: Arc<dyn Tokenizer> =
        Arc::new(TiktokenTokenizer::for_model(graph.llm_client.model_id()));

    let soul = Soul::from_workspace(&graph.workspace)
        .await
        .unwrap_or_else(|_| Soul::custom("You are Aura, an intelligent assistant.".to_string()));
    let system_prompt = soul.system_prompt().to_string();

    let policy = boot::to_execution_policy(&graph.config.agent);
    let token_budget = boot::to_token_budget(&graph.config.agent.context);
    let keep_recent = graph.config.agent.context.keep_recent;
    let auto_snapshot = graph.config.trace.auto_snapshot;
    let snapshot_interval = graph.config.trace.snapshot_interval;

    let (incoming_tx, incoming_rx) = mpsc::channel(buffer);
    let (response_tx, response_rx) = mpsc::channel(buffer);

    let supervisor = AgentSupervisor::new(response_tx);

    let actor_llm_client: Arc<dyn aura_llm::LlmCompletion> = Arc::clone(&graph.llm_client) as _;
    let actor_tool_registry = Arc::clone(&graph.tool_registry);
    let actor_skill_registry = Arc::clone(&graph.skill_registry);
    let actor_tool_executor = Arc::clone(&graph.tool_executor);
    let actor_memory_manager = Arc::clone(&graph.memory_manager);
    let actor_tokenizer = Arc::clone(&tokenizer);
    let actor_hooks = Arc::clone(&graph.hook_manager);
    let actor_trace_store = graph.stores.trace.clone();
    let actor_job_manager = Arc::clone(&graph.job_manager);
    let actor_cost_tracker = Arc::clone(&graph.cost_tracker);
    let actor_skill_assessor = Arc::clone(&graph.skill_assessor);
    let actor_security_gateway = Arc::clone(&graph.security_gateway);

    let router = Router::new(
        Arc::clone(&graph.session_manager),
        supervisor,
        Arc::clone(&graph.channels_registry),
        Arc::clone(&graph.security_gateway),
    )
    .with_actor_spawner(Box::new(move |session, response_tx| {
        let agent_loop = AgentLoop::new(
            Arc::clone(&actor_llm_client),
            Arc::clone(&actor_tool_registry),
            Arc::clone(&actor_skill_registry),
            Arc::clone(&actor_tool_executor),
            ContextManager::new(
                Arc::clone(&actor_tokenizer),
                Box::new(Truncate::new(keep_recent)),
                token_budget.clone(),
            ),
            Arc::clone(&actor_memory_manager),
            policy.clone(),
            Soul::custom(system_prompt.clone()),
            Arc::clone(&actor_security_gateway),
        )
        .with_skill_assessor(Arc::clone(&actor_skill_assessor));

        let trace_collector = Arc::new(Mutex::new(TraceCollector::new(
            &session.id,
            Arc::clone(&actor_trace_store),
            auto_snapshot,
            snapshot_interval,
        )));
        let recorder = Arc::new(ObservabilityRecorder::new(
            Arc::clone(&actor_job_manager),
            trace_collector,
            Arc::clone(&actor_cost_tracker),
        ));

        let actor = AgentActor::new(
            session,
            agent_loop,
            Arc::clone(&actor_tool_executor),
            response_tx,
            Arc::clone(&actor_hooks),
            recorder,
        );
        let (sender, mailbox) = mpsc::channel(buffer);
        tokio::spawn(async move {
            actor.run(mailbox).await;
        });
        sender
    }));

    // Attach cron triggers eagerly — a caller who forgot to plumb the
    // receiver would silently drop every cron-fired turn.
    let cron_trigger_rx = graph
        .cron_trigger_rx
        .take()
        .expect("wire_router called twice; cron_trigger_rx already consumed");
    let router = router.with_cron_triggers(cron_trigger_rx);

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
