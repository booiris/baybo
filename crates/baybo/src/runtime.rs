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

use baybo_agent::actor::AgentActor;
use baybo_agent::agent_loop::{AgentLoop, AgentLoopConfig};
use baybo_agent::router::Router;
use baybo_agent::service::{ShutdownSignal, TaskTracker};
use baybo_agent::supervisor::AgentSupervisor;
use baybo_agent::tool_executor::ToolExecutor;
use baybo_agent::{CronScheduler, CronTriggerEvent, SecretVault, SecurityGateway, SessionManager};
use baybo_channels::{AgentOutput, ChannelRegistry, RouterInbound};
use baybo_config::{BayboConfig, LlmEntryName};
use baybo_context::{ContextManager, ContextManagerConfig, TiktokenTokenizer, Tokenizer};
use baybo_cost::{CostManager, SpendingLimits};
use baybo_llm::BillableLlm;
use baybo_memory::Memory;
use baybo_security::{LeakDetectionRule, LeakDetector};
use baybo_skills::SkillRegistry;
use baybo_skills_assessor::SkillAssessor;
use baybo_storage::Store;
use baybo_tools::ToolRegistry;
use baybo_tools::builtin::DefaultToolsConfig;
use baybo_tools::mcp::{EmbeddedMcpServer, McpReconciler};
use baybo_trace::{SpanRecorder, TraceEventStream};
use baybo_turn::TurnLifecycle;
use regex::Regex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

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
    security: &baybo_config::SecurityConfig,
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
                action: baybo_security::LeakAction::Replace,
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

/// Open the project's sqlite store just far enough to build a
/// [`SecretVault`]. Used by the gateway's vault-only subcommands so
/// they don't have to pay the cost of a full [`build_managers`] call
/// (turn recovery, cron scheduler, tool registry, etc.) to read or
/// rotate a token.
pub async fn build_secret_vault(config: &BayboConfig) -> anyhow::Result<Arc<SecretVault>> {
    let storage = Store::open(boot::storage_db_path(&config.workspace)).await?;
    let master_key = boot::load_encryption_key(&config.security, &storage.secret).await?;
    Ok(Arc::new(SecretVault::new(master_key, storage.secret)))
}

/// Open the sqlite store and return the vault + full [`Store`] handle
/// for CLI subcommands that manage per-channel credentials
/// (`baybo channel list/add/remove`) or per-user pairings
/// (`baybo pair list/approve/revoke`). The returned [`Store`] is cloneable
/// and its fields share a single sqlite connection, so CLI writes land
/// atomically in the same file the gateway reads from.
pub async fn build_bot_registry_deps(
    config: &BayboConfig,
) -> anyhow::Result<(Arc<SecretVault>, Store)> {
    let storage = Store::open(boot::storage_db_path(&config.workspace)).await?;
    let master_key = boot::load_encryption_key(&config.security, &storage.secret).await?;
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
    pub config: Arc<BayboConfig>,
    pub session_manager: Arc<SessionManager>,
    pub turn_lifecycle: Arc<TurnLifecycle>,
    pub cron_scheduler: Arc<CronScheduler>,
    pub security_gateway: Arc<SecurityGateway>,
    pub skill_registry: Arc<SkillRegistry>,
    pub tool_registry: Arc<ToolRegistry>,
    pub tool_executor: Arc<ToolExecutor>,
    /// Deferred supervisor handle for the background-turn manager (built
    /// with the `tool_executor`, before the supervisor exists). `wire_router`
    /// sets it once the supervisor is built so command escorts can route.
    pub bg_supervisor_slot: Arc<std::sync::OnceLock<AgentSupervisor>>,
    /// Subagent profile registry — `wire_router` hands it to a spawned
    /// subagent's `ContextManager`, which resolves the child's system prompt
    /// from it by profile name.
    pub subagent_registry: Arc<baybo_subagent::SubagentRegistry>,
    /// Always already wrapped via `BillableLlm` — every
    /// consumer (main loop, side-LLM in tools, skill_assessor)
    /// shares the same budget gate. Constructed in
    /// [`build_managers`] so a new consumer added downstream can't
    /// accidentally pull a raw `Arc<LlmClient>` and bypass the gate;
    /// the type signature alone is enough to refuse a raw
    /// `Arc<dyn LlmCompletion>` at the call site.
    pub llm_client: Arc<BillableLlm>,
    /// `llm_client` is `pool.default_client()`. The pool exists so the
    /// actor spawner can resolve a per-session pick from
    /// `Session.state.last_llm`.
    pub llm_pool: baybo_agent::LlmPoolHandle,
    /// Owner of cost-record persistence and the budget gate used by
    /// `llm_client` above. Kept on the graph so `wire_router` can
    /// hand it to `AgentLoop` without reconstructing — and so the
    /// gate built into `llm_client` and the records the agent loop
    /// writes are guaranteed to come from the same instance.
    pub cost_manager: Arc<CostManager>,
    /// Live rate-limit knobs shared between the router's `RateLimiter`
    /// and the config-reload `CostReloader`. Created in
    /// [`build_managers`] so both the router wiring and the reloader
    /// hold the same `Arc`.
    pub rate_limit: Arc<baybo_agent::router::LiveRateLimit>,
    /// Config hot-reload orchestrator. Handed to `GatewayDeps` so admin
    /// endpoints + the SIGHUP handler can trigger a reload; owns the
    /// pricing-refresh loop. See `docs/config-hot-reload.md`.
    pub config_reloader: Arc<dyn baybo_gateway::ConfigReloader>,
    pub workspace: Arc<baybo_workspace::WorkspacePaths>,
    pub channels_registry: Arc<ChannelRegistry>,
    pub secret_vault: Arc<SecretVault>,
    /// Deck card manager (`/v1/deck/*` + the DeckCard* tools). Boot and
    /// shutdown are the gateway entrypoint's responsibility.
    pub deck_manager: Arc<baybo_deck::DeckManager>,
    /// Cloneable bundle of every sqlite-backed store handle. Keeping the
    /// whole [`Store`] in one field means adding a new store only
    /// touches [`Store`] itself — the graph and its downstream consumers
    /// pick it up via `stores.xxx` without a new field here.
    pub stores: Store,

    /// Pluggable long-term memory. `None` whenever `config.memory.enabled`
    /// is false or `provider = noop`; a real backend (`mem0` /
    /// `openviking`) is constructed in [`build_managers`] from
    /// `config.memory` + the secret vault. Threaded into every
    /// `AgentLoopConfig` built by the spawner factory in
    /// [`wire_router`]; its `tools()` are registered into the builtin
    /// tool registry at construction time.
    pub memory: Option<Arc<dyn Memory>>,

    /// Consumed by [`wire_router`]. Stored in the graph so the caller
    /// cannot forget to plumb it through — a silently-missing receiver
    /// would make cron-triggered turns disappear into the void. Wrapped
    /// in `Option` so [`wire_router`] can `.take()` it, and calling
    /// `wire_router` twice panics loudly instead of silently handing
    /// out a dummy receiver.
    pub cron_trigger_rx: Option<mpsc::Receiver<CronTriggerEvent>>,

    /// Late-set slot for the actor-backed subagent spawner. The
    /// `spawn_subagent` tool holds a clone (taken at build time, before the
    /// supervisor exists); [`wire_router`] fills it once the spawner is
    /// built. An empty slot at tool-call time surfaces as a failed spawn,
    /// not a panic.
    pub subagent_spawner_slot: baybo_subagent::tool::SubagentSpawnerSlot,

    /// Process-wide parent token for `AgentActor`s. Bridged to the
    /// shared `ShutdownSignal` in [`build_managers`]; cancelling it
    /// cascades down through every actor's per-turn cancel tree.
    pub actor_parent_token: CancellationToken,

    /// Fan-out limiter shared between `spawn_subagent` (reserves at
    /// dispatch) and the router (releases on terminal). The CLI /
    /// observability surface also reads its snapshot. Constructed in
    /// [`build_managers`] so a new consumer always pulls the same
    /// `Arc`; otherwise the limiter on the tool and the one on the
    /// router could diverge silently.
    pub subagent_dispatch_limiter: Arc<dyn baybo_subagent::SubagentDispatchLimiter>,
}

/// Resolve every domain manager, tying them together with the shared
/// `shutdown` signal. Consumes the storage graph (`Store::open`) in the
/// process — the caller keeps only the per-trait `Arc` handles returned
/// inside [`ManagerGraph`].
pub async fn build_managers(
    config: Arc<BayboConfig>,
    // On-disk config path, threaded into the hot-reload orchestrator so
    // it knows what to re-read. `None` disables reload (no file to read).
    config_path: Option<std::path::PathBuf>,
    shutdown: ShutdownSignal,
    leak_detector: Arc<LeakDetector>,
    // Pre-assembled embedded MCP server entries the reconciler should
    // spawn alongside any user-configured `.mcp.json` entries. Built
    // by the boot-path caller (`gateway_cmd::start` for the gateway,
    // empty `Vec` for non-gateway paths) so the manager-graph layer
    // stays free of per-tool-domain wiring (browser blob upload, etc).
    embedded_mcp_servers: Vec<EmbeddedMcpServer>,
) -> anyhow::Result<ManagerGraph> {
    // Shared, hot-swappable Bash permission mode. A `permission` config reload
    // swaps it live (see `reload.rs`); the handle is held by both the tool
    // registry and the reloader.
    let bash_permission = std::sync::Arc::new(baybo_tools::builtin::LivePermissionMode::new(
        boot::to_bash_permission(config.permission),
    ));
    // --- minimal services shared by every boot path
    let workspace_paths =
        baybo_workspace::WorkspacePaths::new(std::path::PathBuf::from(&config.workspace.path));
    let workspace_root = workspace_paths.root().to_path_buf();
    // Best-effort: warm the workspace-scoped uv python cache so the
    // first agent-issued `python …` call doesn't stall on a cold
    // cpython download. Runs detached; failure is logged and swallowed.
    baybo_tools::builtin::bash::spawn_uv_python_prewarm(&workspace_paths);
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
    let subagent_registry = {
        let reg = Arc::new(baybo_subagent::SubagentRegistry::new());
        let builtins = reg.register_builtins();
        if builtins > 0 {
            info!(count = builtins, "registered built-in subagent profiles");
        }
        let workspace_agents = workspace_paths.agents_dir();
        let loaded = reg.load_dir(&workspace_agents);
        if loaded > 0 {
            info!(
                count = loaded,
                path = %workspace_agents.display(),
                "loaded subagent profiles from workspace"
            );
        }
        reg
    };
    let workspace = Arc::new(workspace_paths.clone());
    let channels_registry = Arc::new(ChannelRegistry::new());
    // Eagerly install every enabled channel from config so connections
    // can attach to a pre-existing slot rather than racing on lazy
    // first-connect creation. See docs/modules/channels-protocol-refactor.md.
    baybo_gateway::channel::boot::install_channels(&channels_registry, &config.channels)?;

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
    // below (cron) can `register` directly. Wrapped in
    // `Arc` once all registrations are done — that way the "single
    // owner" invariant the registrations relied on is enforced by the
    // type system, not by an `Arc::get_mut().expect()` runtime check.
    // Vault is constructed up here (before `build_llm_client`) so the
    // openai-subscription provider can read its OAuth bundle straight away.
    // Other providers ignore the vault. Can't route through
    // `build_secret_vault` here — it would re-open sqlite; load the key
    // directly and share `stores.secret` across both call sites.
    let master_key = boot::load_encryption_key(&config.security, &stores.secret).await?;
    let secret_vault = Arc::new(SecretVault::new(master_key, stores.secret.clone()));

    // CostManager built before the LLM client so its gate closure is
    // ready for `boot::build_llm_client` to seal into `BillableLlm`.
    // The provider registry is shared between pricing harvest and
    // `build_llm_client` — single source of truth for factories.
    let provider_registry = baybo_llm::LlmProviderRegistry::with_default_providers();
    let spending_limits = SpendingLimits {
        daily_usd: config.cost.spending_limits.daily_usd,
        monthly_usd: config.cost.spending_limits.monthly_usd,
    };
    // Seeded empty: the budget gate is primed from the built clients' own
    // (snapshot-derived) pricing once the pool is up — see below.
    let cost_manager = CostManager::new(
        stores.cost.clone(),
        std::collections::HashMap::new(),
        spending_limits,
    );
    cost_manager.hydrate().await;

    // Live rate-limit knobs, shared between the router and the
    // config-reload `CostReloader` so `cost.rate_limit` edits apply
    // without rebuilding the router.
    let rate_limit = baybo_agent::router::LiveRateLimit::new(
        config.cost.rate_limit.max_requests,
        std::time::Duration::from_secs(config.cost.rate_limit.window_secs),
    );

    // Build one client per `config.llm` entry (default failure aborts
    // boot; non-default failures drop with a warn). Shared with the
    // hot-reload path — see `crate::reload::build_pool_clients`.
    let built = crate::reload::build_pool_clients(
        &config,
        &provider_registry,
        stores.blob.clone(),
        Arc::clone(&secret_vault),
        &cost_manager,
    )
    .await?;
    // Prime the budget gate offline from each built client's own
    // (snapshot-derived) pricing, keyed by `model_info.id` to match the
    // cost lookup. The refresh loop (owned by the reloader below)
    // overlays live prices.
    cost_manager.merge_pricings(crate::reload::pricing_overlay(&built));
    let pool = baybo_agent::LlmClientPool::from_config(baybo_agent::LlmPoolConfig {
        clients: built.clients,
        overrides: built.overrides,
        entry_models: built.entry_models,
        lite: built.lite,
        default_name: config.default_llm.clone(),
        tier_map: config.agent.model_tiers.clone(),
    })
    .map_err(|e| anyhow::anyhow!("build LLM client pool: {e}"))?;
    let llm_client = pool.default_client();
    let info = llm_client.model_info();
    info!(
        provider = %info.provider,
        model = %info.id,
        pool_entries = %pool
            .entry_names()
            .iter()
            .map(LlmEntryName::as_str)
            .collect::<Vec<_>>()
            .join(", "),
        supports_vision = info.supports_vision,
        "configured LLM client pool"
    );
    // Wrapped in a swap handle so a config reload can atomically replace
    // the whole pool; in-flight turns keep the `Arc` they already cloned.
    let llm_pool: baybo_agent::LlmPoolHandle = Arc::new(parking_lot::RwLock::new(Arc::new(pool)));

    // Resolve the egress proxy once for every HTTP consumer below: the LLM
    // pricing-refresh loop, the WebFetch client, and MCP transports/children.
    let proxy = boot::proxy_settings(&config);
    let tool_proxy = proxy
        .as_ref()
        .map(|p| p.to_proxy())
        .transpose()
        .map_err(|e| anyhow::anyhow!("invalid proxy.url: {e}"))?;

    // Hot-reload orchestrator. The `LlmReloader` owns the pricing-refresh
    // loop from here on (it spawns the initial one), so a reload can
    // cancel + respawn it against the new model set.
    let config_reloader: Arc<dyn baybo_gateway::ConfigReloader> = {
        let llm_reloader = crate::reload::LlmReloader::new(
            Arc::clone(&llm_pool),
            Arc::clone(&cost_manager),
            stores.blob.clone(),
            Arc::clone(&secret_vault),
            shutdown.clone(),
            crate::reload::refresh_pairs(&config),
            proxy.clone(),
        );
        let cost_reloader =
            crate::reload::CostReloader::new(Arc::clone(&cost_manager), Arc::clone(&rate_limit));
        Arc::new(crate::reload::RuntimeConfigReloader::new(
            config_path,
            baybo_config::ConfigHandle::new(Arc::clone(&config)),
            llm_reloader,
            cost_reloader,
            Arc::clone(&bash_permission),
        ))
    };

    let mut tool_registry = ToolRegistry::with_defaults(DefaultToolsConfig {
        blob_store: stores.blob.clone(),
        workspace_paths: baybo_workspace::WorkspacePaths::new(workspace_root.clone()),
        proxy: tool_proxy,
        permission: Arc::clone(&bash_permission),
        builtin_memory: config.memory.builtin.enabled,
    });

    let assessment_mode = boot::to_assessment_mode(config.skills.risk_check);
    // Bind once to the reserved system bucket: skill assessment is
    // platform safety overhead, not user-attributable work, and its
    // verdicts are cached by content hash, so it pins the boot-time
    // default rather than following hot-reload swaps.
    let assessor_llm = Arc::new(llm_client.bind(baybo_llm::Attribution::system("skill-assessor")));
    let skill_assessor = Arc::new(SkillAssessor::with_background_worker(
        assessor_llm,
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
        stores.session_folder.clone(),
    ));

    let turn_lifecycle = Arc::new(TurnLifecycle::new(stores.turn.clone()));

    // Crash recovery: roll forward orphan trace rows (steps/spans left
    // `Pending` because their `with_step` future was dropped before
    // close) and the surrounding non-terminal turns. `ended_at` is
    // backfilled from observed activity, not boot wall-clock.
    // Best-effort — errors warn but never block boot.
    baybo_agent::recovery::recover_orphaned_traces_and_turns(
        stores.trace.clone(),
        Arc::clone(&turn_lifecycle),
    )
    .await;

    // CostTracker has been retired in favour of a process-wide
    // --- cron scheduler (built before ToolExecutor so its tools register
    // while `tool_registry` still has a single Arc owner)
    let (cron_trigger_tx, cron_trigger_rx) = mpsc::channel(64);
    let cron_scheduler = Arc::new(CronScheduler::new(
        stores.cron.clone(),
        cron_trigger_tx,
        Arc::new(shutdown.clone()) as Arc<dyn baybo_cron::Shutdown>,
    ));
    // Late-set slot for the actor-backed subagent spawner. The
    // `spawn_subagent` tool (built below while the registry is assembled)
    // gets a clone now and reads it on first use; the runtime fills it in
    // `wire_router`, once the supervisor + actor spawner exist.
    let subagent_spawner_slot: baybo_subagent::tool::SubagentSpawnerSlot =
        Arc::new(std::sync::OnceLock::new());
    for (tool, manifest) in baybo_cron::tools::agent_tools(Arc::clone(&cron_scheduler)) {
        tool_registry.register(tool, manifest);
    }

    // The dream pass is a cron job like any other, so it is seeded through
    // the same scheduler the model's own jobs go through. It belongs to the
    // owner (there is no conversation that asked for it) and is reconciled,
    // not re-asserted: see `ensure_dream_job` for who wins over whom.
    // Each runtime-owned job is seeded and reconciled here; adding another
    // is one more call beside this one.
    if let Err(e) = cron_scheduler
        .ensure_builtin_job(baybo_cron::BuiltinJobSpec {
            job: baybo_model::BuiltinCronJob::Dream,
            enabled: config.memory.builtin.enabled,
            schedule: config.memory.builtin.dream.schedule.clone(),
            timezone: baybo_cron::host_timezone(),
            user_id: baybo_gateway::auth::OWNER_USER_ID.to_string(),
            channel: baybo_model::ChannelType::owner(),
        })
        .await
    {
        // A bad schedule in config must not take the process down — every
        // other feature still works without the nightly tidy-up.
        tracing::warn!(error = %e, "failed to seed a built-in cron job");
    }

    // Planning-checklist tools (`Task*`). Like cron's, they hold
    // an `Arc<dyn Store>` (here the per-session `TaskStore`) so they register
    // from the runtime rather than `default_tools`. The agent loop re-injects
    // the live list each turn (see `AgentLoop::refresh_task_reminder`).
    for (tool, manifest) in baybo_task::tools::agent_tools(stores.task.clone()) {
        tool_registry.register(tool, manifest);
    }

    // `spawn_subagent` is just another tool from the LLM's perspective.
    // It calls the actor-backed `SubagentSpawner` directly (via the
    // late-set slot above) to materialise the child; the spawner does the
    // session-create + actor-spawn + wait. The fan-out limiter is shared
    // between the tool (reserves at dispatch) and the spawner (releases on
    // terminal) — both hold the same `Arc<FanOutLimiter>`.
    let subagent_dispatch_limiter: Arc<dyn baybo_subagent::SubagentDispatchLimiter> =
        Arc::new(baybo_subagent::FanOutLimiter::new());
    {
        let (tool, manifest) =
            baybo_subagent::tool::make(baybo_subagent::tool::SpawnSubagentToolConfig {
                spawner: Arc::clone(&subagent_spawner_slot),
                registry: Arc::clone(&subagent_registry),
                sessions: Arc::clone(&session_manager),
                dispatch_limiter: Arc::clone(&subagent_dispatch_limiter),
                max_depth: config.agent.max_subagent_depth,
                max_subagents_per_root: config.agent.max_subagents_per_root,
            });
        tool_registry.register(tool, manifest);
    }

    // --- Skill tool — registered with the risk assessor as the
    // gate. Lives in baybo-skills (parallel to baybo-cron::tools)
    // because it needs the registry + assessor; both are constructed
    // above. Always registered: when the registry is empty the
    // per-turn system reminder is suppressed and the LLM never tries
    // the call.
    {
        let risk_check: Arc<dyn baybo_skills::SkillRiskCheck> = Arc::clone(&skill_assessor) as _;
        let (tool, manifest) =
            baybo_skills::build_skill_tool(Arc::clone(&skill_registry), Arc::clone(&risk_check));
        tool_registry.register(tool, manifest);

        let (install_tool, install_manifest) = baybo_skills::tools::build_install_tool(
            workspace_paths.skills_dir(),
            Arc::clone(&skill_registry),
            risk_check,
        );
        tool_registry.register(install_tool, install_manifest);

        let (uninstall_tool, uninstall_manifest) = baybo_skills::tools::build_uninstall_tool(
            workspace_paths.skills_dir(),
            Arc::clone(&skill_registry),
        );
        tool_registry.register(uninstall_tool, uninstall_manifest);
    }

    // --- Deck manager (docs/modules/deck.md) — built here so its tools
    // register while `tool_registry` still has a single Arc owner. Push
    // hooks ride the owner channel; boot (service start + post-upgrade
    // re-gates) and shutdown are driven by the gateway entrypoint.
    let deck_manager = baybo_deck::DeckManager::from_config(baybo_deck::DeckManagerConfig {
        store: stores.deck.clone(),
        vault: Arc::clone(&secret_vault),
        events: Arc::new(baybo_gateway::deck_events::GatewayDeckEvents::new(
            Arc::clone(&channels_registry),
        )),
        blob: stores.blob.clone(),
        deck_root: workspace_paths.deck_dir(),
        scratch_root: workspace_paths.state_dir().join("deck-scratch"),
    });
    for (tool, manifest) in baybo_deck::tools::agent_tools(Arc::clone(&deck_manager)) {
        tool_registry.register(tool, manifest);
    }

    // --- security gateway + tool executor
    // Tool-output spill now lives in `baybo-context` (resolved from the
    // session's workspace handle); the gateway only does scan + sanitize.
    let security_gateway = Arc::new(SecurityGateway::new(
        Arc::clone(&leak_detector),
        Arc::clone(&secret_vault),
    ));
    let gate_map = channels_registry.approval_gates();
    let sandbox_boot = crate::sandbox_boot::resolve_sandbox_runner(config.permission).await?;

    // --- pluggable memory backend. Constructed here while
    // `tool_registry` is still mutable so the impl's `tools()` can be
    // registered as builtins. `None` (the no-op path) when
    // `config.memory.enabled = false` or `provider = noop`.
    let memory =
        baybo_memory::boot::build_memory_backend(&config, &secret_vault, proxy.as_ref()).await;
    // Benchmark-only: wrap so recall + tools stay live but writes are no-ops, so
    // the memory bench can drive the real agent without QA turns polluting the
    // recall scope. Compiled out of production builds.
    #[cfg(feature = "bench-readonly-memory")]
    let memory = memory.map(|m| {
        tracing::warn!(
            "bench-readonly-memory: memory writes disabled (recall + tools only) — benchmark build"
        );
        Arc::new(baybo_memory::ReadOnlyMemory::new(m)) as Arc<dyn baybo_memory::Memory>
    });
    if let Some(m) = &memory {
        for (tool, manifest) in m.tools() {
            tool_registry.register(tool, manifest);
        }
    }

    // Freeze the registry now that mutation is done; downstream
    // consumers (`tool_executor`, `McpReconciler`, the actor spawner)
    // need an `Arc<ToolRegistry>` for sharing across tasks.
    let tool_registry = Arc::new(tool_registry);

    // The opener for deferred tools. Registered against the frozen `Arc`
    // because it reads the live registry: a deferred sidecar's tools appear
    // when it connects, which is after this point.
    {
        let (tool, manifest) = baybo_tools::builtin::tool_search::make(Arc::clone(&tool_registry));
        tool_registry.register_dynamic("tool-search", tool, manifest);
    }

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
    // Resolver consulted on `Read` before the filesystem: serves the session
    // transcript from the store for post-compaction recovery, with per-session
    // access control. `ReadTool` consults it; `None` would disable virtual reads.
    let virtual_reads: Option<Arc<dyn baybo_tools::VirtualReadResolver>> =
        Some(Arc::new(baybo_agent::SessionTranscriptReader::new(
            Arc::clone(&session_manager) as Arc<dyn baybo_agent::SessionTranscript>,
            workspace_paths.clone(),
        )));

    let bg_output_dir = baybo_tools::builtin::bash::background_output_dir(&workspace_paths);
    let bg_groups_dir = baybo_tools::builtin::bash::detached_group_dir(&bg_output_dir);

    // Reap detached command groups this host orphaned across a hard kill
    // (SIGKILL / OOM / crash) — those run neither the `ProcessGroupKiller`
    // destructors nor the force-exit reap, so the on-disk ledger is the only
    // record left. Verified against pid-group recycling before any SIGKILL.
    // Also prune stale detached-command output files (the agent reads them
    // shortly after the completion notification, so a week's retention is
    // generous). Off the boot path — blocking fs + a /proc scan.
    {
        let output_dir = bg_output_dir.clone();
        let groups_dir = bg_groups_dir.clone();
        tokio::task::spawn_blocking(move || {
            let reaped = baybo_tools::builtin::bash::reap_persisted_detached_groups(&groups_dir);
            if reaped > 0 {
                info!(
                    reaped,
                    "reaped orphaned background command groups from a prior run"
                );
            }
            let pruned = baybo_tools::builtin::bash::prune_background_outputs(
                &output_dir,
                std::time::Duration::from_secs(7 * 24 * 60 * 60),
            );
            if pruned > 0 {
                info!(pruned, "pruned stale background-command output files");
            }
        });
    }

    // Background-turn sink for "Bash timeout → background". The manager
    // routes completion notifications via the supervisor, which is built
    // further down, so it holds it behind a `OnceLock` set right after.
    let bg_supervisor_slot = Arc::new(std::sync::OnceLock::new());
    let bg_manager = Arc::new(baybo_agent::BackgroundJobManager::new(
        Arc::clone(&bg_supervisor_slot),
        bg_groups_dir,
        shutdown.cancellation_token(),
    ));
    let background_jobs: Option<Arc<dyn baybo_tools::BackgroundJobSink>> = Some(bg_manager.clone());
    let background_control: Option<Arc<dyn baybo_tools::BackgroundJobControl>> = Some(bg_manager);

    // `ToolExecutor` doesn't store an LLM handle; the active
    // `BillableLlm` is passed in per `execute` call and bound to the
    // tool span there, so a tool's side-LLM call bills against the
    // model the surrounding actor is currently using.
    let tool_executor = Arc::new(ToolExecutor::new(
        Arc::clone(&tool_registry),
        gate_map,
        Arc::clone(&security_gateway),
        sandbox_root,
        workspace_paths.clone(),
        sandbox_boot.runner,
        sandbox_boot.bypass_reason,
        virtual_reads,
        background_jobs,
        background_control,
    ));

    // --- MCP reconciler — re-reads <workspace>/.mcp.json every 5s and
    // dynamically registers/unregisters MCP-discovered tools. Bridge the
    // shared `ShutdownSignal` to a `CancellationToken` since the
    // reconciler lives in `baybo-tools`, which doesn't depend on
    // `baybo-agent`.
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
        proxy.clone(),
    );
    mcp_reconciler.spawn();

    Ok(ManagerGraph {
        config,
        session_manager,
        turn_lifecycle,
        cron_scheduler,
        security_gateway,
        bg_supervisor_slot,
        skill_registry,
        tool_registry,
        tool_executor,
        subagent_registry,
        llm_client,
        llm_pool,
        cost_manager,
        rate_limit,
        config_reloader,
        workspace,
        channels_registry,
        secret_vault,
        deck_manager,
        stores,
        memory,
        cron_trigger_rx: Some(cron_trigger_rx),
        subagent_spawner_slot,
        actor_parent_token,
        subagent_dispatch_limiter,
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
    pub incoming_tx: mpsc::Sender<RouterInbound>,
    pub incoming_rx: mpsc::Receiver<RouterInbound>,
    pub response_rx: mpsc::Receiver<AgentOutput>,
    /// Clone of the actor registry, handed to the gateway so the chat
    /// `PUT /v1/chat/sessions/{id}/model` endpoint can route an
    /// `AgentMessage::SetModel` to a live actor (re-pin in place) the
    /// same way `/stop` reaches one. Cheap to clone — backed by an
    /// `Arc<DashMap>`.
    pub supervisor: AgentSupervisor,
}

/// Build the [`Router`] and wire a per-session actor spawner against
/// the graph. The returned handle already has cron triggers attached —
/// every cron-enqueued session reaches the router regardless of entry
/// point.
pub async fn wire_router(graph: &mut ManagerGraph) -> RouterRunHandle {
    let buffer = graph.config.channels.message_buffer_size;

    let tokenizer: Arc<dyn Tokenizer> = Arc::new(TiktokenTokenizer::for_model(
        graph.llm_client.model_info().id.as_str(),
    ));

    let token_calibration = Arc::new(baybo_context::TokenCalibration::new());

    let max_iterations = graph.config.agent.max_iterations;
    let compression_threshold = graph.config.agent.context.compression_threshold;
    let keep_recent = graph.config.agent.context.keep_recent;
    let builtin_memory = graph.config.memory.builtin.enabled;

    let (incoming_tx, incoming_rx) = mpsc::channel(buffer);
    let (response_tx, response_rx) = mpsc::channel(buffer);

    let supervisor = AgentSupervisor::new(response_tx);
    // Hand the now-built supervisor to the background-turn manager so its
    // escorts can route completion notifications back to parent sessions.
    let _ = graph.bg_supervisor_slot.set(supervisor.clone());

    // Process-wide trace event bus. Stays for trace observers
    // (WebUI live stream etc.); `CostManager` no longer subscribes
    // — agent_loop calls it directly with the token counts it
    // already has.
    let trace_event_stream = TraceEventStream::new();

    // `cost_manager` and the guarded `llm_client` are both built in
    // `build_managers` and live on `graph` — every consumer (main
    // loop, side-LLM in tools, skill_assessor) shares the same gate.
    // Re-binding here just to keep the local name changes below
    // minimal.
    let cost_manager = Arc::clone(&graph.cost_manager);
    let llm_pool = Arc::clone(&graph.llm_pool);

    // Single `Arc`-shared actor factory: used by the router for top-level
    // user/cron actors and by the subagent spawner for child actors.
    let spawn_actor_for: baybo_agent::router::ActorSpawner = {
        let llm_pool = Arc::clone(&llm_pool);
        let tool_registry = Arc::clone(&graph.tool_registry);
        let skill_registry = Arc::clone(&graph.skill_registry);
        let tool_executor = Arc::clone(&graph.tool_executor);
        let trace_store = graph.stores.trace.clone();
        let task_store = graph.stores.task.clone();
        let turn_lifecycle = Arc::clone(&graph.turn_lifecycle);
        let security_gateway = Arc::clone(&graph.security_gateway);
        let tokenizer = Arc::clone(&tokenizer);
        let trace_event_stream = trace_event_stream.clone();
        let token_calibration = Arc::clone(&token_calibration);

        let sessions = Arc::clone(&graph.session_manager);
        // The actor stamps a one-shot cron result's delivery here once it has
        // appended it to the scheduling conversation.
        let cron_store_for_spawn = graph.stores.cron.clone();
        let subagent_registry = Arc::clone(&graph.subagent_registry);
        let workspace_paths_arc = Arc::new(baybo_workspace::WorkspacePaths::new(
            graph.workspace.root().to_path_buf(),
        ));
        let supervisor_for_spawn = supervisor.clone();
        // Memory subsystem. Constructed in `build_managers` via
        // `baybo_memory::boot::build_memory_backend` so the impl's
        // `tools()` could be registered as builtins while `tool_registry`
        // was still mutable. Threaded into every actor's `AgentLoopConfig`
        // below; `None` keeps the no-op path (every memory hook skipped,
        // nothing billed).
        let memory: Option<Arc<dyn Memory>> = graph.memory.clone();
        // Shared across every actor: the title pass notifies this sink once it
        // has persisted a generated title, and it fans a `SessionUpdated` patch
        // out to every Subscribed channel so the owning surface updates live.
        let title_sink: Option<Arc<dyn baybo_agent::SessionTitleSink>> = Some(Arc::new(
            baybo_gateway::channel::session_title::SessionTitleBroadcaster::new(Arc::clone(
                &graph.channels_registry,
            )),
        ));
        Arc::new(
            move |session: baybo_model::Session,
                  initial_llm: Option<LlmEntryName>,
                  initial_model: Option<String>,
                  initial_effort: Option<String>,
                  response_tx: mpsc::Sender<AgentOutput>,
                  actor_token: CancellationToken| {
                // `initial_llm` is `Some` either when the router's
                // subagent handler resolved a model for a child (from the
                // spawn request's `model_tier`) or when a user-channel
                // session carries a per-session pin in
                // `session.state.last_llm` (the chat model switch) that
                // `handle_incoming` forwarded here; `None` ⇒ pool default.
                // `initial_model` is the chat pick WITHIN that entry
                // (`session.state.last_model`); `None` ⇒ entry default.

                // The overlay is loaded lazily, here, because the set of
                // agents is DB state: boot cannot enumerate the persona
                // folders to scan, and nothing else populates the map that
                // `summaries_for` / `get_scoped` read.
                let agent_id = session.state.agent_id_or_builtin();
                skill_registry.ensure_agent_overlay(&agent_id, &workspace_paths_arc);

                let agent_loop = AgentLoop::from_config(AgentLoopConfig {
                    llm_pool: Arc::clone(&llm_pool),
                    initial_llm,
                    initial_model,
                    initial_effort,
                    tool_registry: Arc::clone(&tool_registry),
                    tool_executor: Arc::clone(&tool_executor),
                    context_manager: ContextManager::from_config(ContextManagerConfig {
                        tokenizer: Arc::clone(&tokenizer),
                        workspace: Arc::clone(&workspace_paths_arc),
                        keep_recent,
                        compression_threshold,
                        calibration: Arc::clone(&token_calibration),
                        skill_registry: Arc::clone(&skill_registry),
                        channel: session.channel.clone(),
                        session_id: session.id.clone(),
                        sessions: Arc::clone(&sessions),
                        subagent_profile: session
                            .state
                            .subagent_type
                            .clone()
                            .map(|name| (Arc::clone(&subagent_registry), name)),
                        // One field feeds both the persona arm and the skill
                        // scope, so a session can never run one agent's soul
                        // with another's skills.
                        agent: Some(agent_id),
                        builtin_memory,
                    }),
                    max_iterations,
                    security_gateway: Arc::clone(&security_gateway),
                    sessions: Some(Arc::clone(&sessions)),
                    memory: memory.clone(),
                    task_store: task_store.clone(),
                    title_sink: title_sink.clone(),
                });

                let span_recorder = Arc::new(SpanRecorder::new(
                    session.id.clone(),
                    session.user.id.clone(),
                    Arc::clone(&trace_store),
                    trace_event_stream.clone(),
                ));

                let runner_ctx = baybo_agent::runner::ActorRunnerContext {
                    session_id: session.id.clone(),
                    user_id: session.user.id.clone(),
                    channel: session.channel.clone(),
                    response_tx: response_tx.clone(),
                    turn_lifecycle: Arc::clone(&turn_lifecycle),
                    trace_store: trace_store.clone(),
                };
                let actor = AgentActor::from_parts(
                    baybo_agent::state::DurableActorState::new(session),
                    baybo_agent::state::VolatileResources {
                        agent_loop,
                        response_tx,
                        turn_lifecycle: Arc::clone(&turn_lifecycle),
                        span_recorder,
                        actor_token,
                        supervisor: Some(supervisor_for_spawn.clone()),
                        session_manager: Arc::clone(&sessions),
                        cron_store: Arc::clone(&cron_store_for_spawn),
                    },
                );
                let (sender, mailbox) =
                    baybo_agent::mailbox::channel(baybo_agent::mailbox::DEFAULT_CAPACITY);
                baybo_agent::runner::spawn_actor(actor, mailbox, runner_ctx);
                sender
            },
        )
    };

    // `take` the cron rx eagerly — a caller who forgot to plumb it would
    // silently drop every cron-fired turn. Calling `wire_router` twice
    // panics here loudly rather than silently handing out a dummy receiver.
    let cron_trigger_rx = graph
        .cron_trigger_rx
        .take()
        .expect("wire_router called twice; cron_trigger_rx already consumed");

    // Idle actor reaper: shuts down registered actors whose sessions
    // have been idle. Hydration on the next user message rebuilds the
    // actor from the durable session row, so eviction is lossless for
    // an actor with no in-flight turn. The session row itself is never
    // deleted.
    baybo_agent::supervisor::spawn_idle_reaper(
        supervisor.clone(),
        Arc::clone(&graph.session_manager),
        graph.actor_parent_token.clone(),
    );

    // Turn-state projector: the single producer of the web chat's live
    // turn-activity signal. It subscribes to turn lifecycle start + terminal
    // events and, on each transition, broadcasts the session's current
    // `TurnState` recomputed from the turn store. See
    // `spawn_turn_state_projector`.
    baybo_agent::supervisor::spawn_turn_state_projector(
        Arc::clone(&graph.turn_lifecycle),
        Arc::clone(&graph.session_manager),
        supervisor.response_tx().clone(),
        graph.actor_parent_token.clone(),
    );

    let workspace_paths_for_router = Arc::new(baybo_workspace::WorkspacePaths::new(
        graph.workspace.root().to_path_buf(),
    ));

    // External-agent registry: only kinds the operator has explicitly
    // opted into (`enabled: true` in baybo.json) are probed and
    // registered. An installed-but-not-enabled binary on PATH is
    // NOT a trust signal — registration would expose the LLM to a
    // host-execution channel that bypasses baybo's sandbox + approval
    // gate.
    let external_agents = Arc::new(baybo_agent::external_agent::build_registry(
        graph.config.external_agents.boot_entries(),
        boot::proxy_settings(&graph.config),
    ));

    // Actor-backed subagent spawner: the `spawn_subagent` tool calls it
    // directly via the slot wired at build time. Built here — after the
    // supervisor + actor spawner exist — sharing the fan-out limiter with
    // the tool plus the same actor-spawn closure the router's cron path
    // uses. A double-set is impossible: the cron `take` above panics first
    // if `wire_router` runs twice.
    let _ = graph.subagent_spawner_slot.set(Arc::new(
        baybo_agent::runtime::subagent_spawner::ActorSubagentSpawner::from_config(
            baybo_agent::runtime::subagent_spawner::SubagentSpawnerConfig {
                session_manager: Arc::clone(&graph.session_manager),
                supervisor: supervisor.clone(),
                dispatch_limiter: Arc::clone(&graph.subagent_dispatch_limiter),
                cost_manager: Arc::clone(&cost_manager),
                turn_lifecycle: Arc::clone(&graph.turn_lifecycle),
                actor_parent_token: graph.actor_parent_token.clone(),
                external_agents: Arc::clone(&external_agents),
                llm_pool: Arc::clone(&llm_pool),
                workspace_paths: Arc::clone(&workspace_paths_for_router),
                actor_spawner: Arc::clone(&spawn_actor_for),
            },
        ),
    ));

    // Hand a clone to the gateway before the router takes ownership, so
    // the chat model-switch endpoint can re-pin live actors.
    let supervisor_for_gateway = supervisor.clone();
    let router = Router::from_config(baybo_agent::router::RouterConfig {
        agent_profiles: graph.stores.agent_profile.clone(),
        workspace: Arc::clone(&workspace_paths_for_router),
        session_manager: Arc::clone(&graph.session_manager),
        supervisor,
        channels: Arc::clone(&graph.channels_registry),
        security_gateway: Arc::clone(&graph.security_gateway),
        cost_manager: Arc::clone(&cost_manager),
        actor_spawner: spawn_actor_for,
        turn_lifecycle: Arc::clone(&graph.turn_lifecycle),
        cron_store: graph.stores.cron.clone(),
        cron_trigger_rx,
        actor_parent_token: graph.actor_parent_token.clone(),
        rate_limit: Arc::clone(&graph.rate_limit),
    });

    RouterRunHandle {
        router,
        incoming_tx,
        incoming_rx,
        response_rx,
        supervisor: supervisor_for_gateway,
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
            "baybo: graceful shutdown exceeded {}s, force-exiting",
            budget.as_secs()
        );
        // `process::exit` runs no destructors, so the `ProcessGroupKiller`
        // guards that reap detached `Bash` command trees never fire. Reap
        // them explicitly or they orphan to init and leak until the next
        // reboot.
        baybo_tools::builtin::bash::reap_tracked_process_groups();
        std::process::exit(0);
    });
}
