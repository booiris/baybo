mod boot;
mod singleton;
mod tui_log;

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
    CronScheduler, JobManager, MemoryManager, SecretVault, SecurityGateway, SessionManager,
    TraceCollector,
};
use aura_channels::{ChannelRegistry, TuiAdapter, TuiLogSink};
use aura_cli::cli::ShellKind;
use aura_cli::{
    Cli, CliDashboardProvider, CliSlashHandler, Commands, ContextBuilder, Invocation, OutputFormat,
    dispatch,
};
use aura_context::{ContextManager, TiktokenTokenizer, Tokenizer, Truncate};
use aura_hook::HookManager;
use aura_security::EncryptionKey;
use aura_skills::SkillRegistry;
use aura_storage::Store;
use aura_tools::ToolRegistry;
use aura_workspace::WorkspaceManager;
use clap::CommandFactory;
use clap::Parser;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::{Mutex, RwLock, mpsc};
use tracing::{error, info};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::tui_log::TuiLogLayer;

struct SecondPrecisionTimer;

impl FormatTime for SecondPrecisionTimer {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z"))
    }
}

enum TracingMode<'a> {
    /// argv / one-shot command path: everything to stdout, no TUI echo.
    Stdout,
    /// Chat path: fmt layer writes rolling file under `<log_dir>/aura.log`,
    /// plus a warn/error echo layer feeding the TUI scrollback via the
    /// returned `OnceLock<TuiLogSink>`.
    Chat { log_dir: &'a Path },
}

struct ChatTracing {
    _file_guard: WorkerGuard,
    tui_sink: Arc<OnceLock<TuiLogSink>>,
}

fn init_tracing(mode: TracingMode<'_>) -> Option<ChatTracing> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("aura=info"));
    let json = std::env::var("AURA_LOG_FORMAT").unwrap_or_default() == "json";

    match mode {
        TracingMode::Stdout => {
            let fmt_layer = fmt::layer()
                .with_timer(SecondPrecisionTimer)
                .with_target(true)
                .with_file(true)
                .with_line_number(true);
            if json {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(fmt_layer.json().with_span_list(true))
                    .init();
            } else {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(fmt_layer)
                    .init();
            }
            None
        }
        TracingMode::Chat { log_dir } => {
            if let Err(e) = std::fs::create_dir_all(log_dir) {
                eprintln!(
                    "warning: could not create log dir {}: {e}. Falling back to stdout logging.",
                    log_dir.display()
                );
                return init_tracing(TracingMode::Stdout);
            }
            let appender = tracing_appender::rolling::daily(log_dir, "aura.log");
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let tui_sink: Arc<OnceLock<TuiLogSink>> = Arc::new(OnceLock::new());
            let tui_layer = TuiLogLayer::new(Arc::clone(&tui_sink));
            let fmt_layer = fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .with_timer(SecondPrecisionTimer)
                .with_target(true)
                .with_file(true)
                .with_line_number(true);
            if json {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(fmt_layer.json().with_span_list(true))
                    .with(tui_layer)
                    .init();
            } else {
                tracing_subscriber::registry()
                    .with(env_filter)
                    .with(fmt_layer)
                    .with(tui_layer)
                    .init();
            }
            Some(ChatTracing {
                _file_guard: guard,
                tui_sink,
            })
        }
    }
}

/// Resolve the effective aura.json path, if any, for display in diagnostics.
fn resolve_config_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("AURA_CONFIG_PATH") {
        return Some(PathBuf::from(explicit));
    }
    let default = PathBuf::from("aura.json");
    default.exists().then_some(default)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // `completion` is the only subcommand that must work without loading
    // config or initialising tracing — it is pure stdout output.
    if let Some(Commands::Completion { shell }) = cli.command {
        print_completion(shell)?;
        return Ok(());
    }

    // Bare `aura` (no subcommand) prints help and exits. The interactive
    // chat loop is reached via the explicit `aura tui` subcommand so the
    // default invocation doesn't surprise users with a full-screen app.
    if cli.command.is_none() {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    }

    let cli_format = pick_format(&cli);

    let config = boot::load_config().await?;
    let config = Arc::new(config);
    let buffer = config.channels.message_buffer_size;

    let chat_mode = matches!(cli.command, Some(Commands::Tui));
    let workspace_root = PathBuf::from(&config.workspace.path);
    let log_dir = workspace_root.join("logs");
    let chat_tracing = if chat_mode {
        init_tracing(TracingMode::Chat { log_dir: &log_dir })
    } else {
        init_tracing(TracingMode::Stdout)
    };

    info!("Aura - Intelligent Assistant Framework starting");

    // --- minimal services required by both argv and chat modes ---

    let skill_registry = {
        let mut reg = SkillRegistry::new();
        let workspace_skills = workspace_root.join("skills");
        let workspace_loaded = reg.load_dir(&workspace_skills);
        if workspace_loaded > 0 {
            info!(
                count = workspace_loaded,
                path = %workspace_skills.display(),
                "loaded skills from workspace"
            );
        }
        Arc::new(reg)
    };
    let tool_registry = Arc::new(ToolRegistry::new());
    let workspace = Arc::new(WorkspaceManager::new(PathBuf::from(&config.workspace.path)));
    let channels_registry = Arc::new(RwLock::new(ChannelRegistry::new()));

    // LLM client is required for the chat loop but optional for argv commands
    // that only inspect state. Argv mode logs and continues; chat mode errors.
    let llm_client = match boot::build_llm_client(&config.llm) {
        Ok(c) => {
            let client = Arc::new(c);
            info!(
                provider = %client.model_info().provider,
                model = %client.model_id(),
                "configured LLM client"
            );
            Some(client)
        }
        Err(e) if cli.command.is_some() => {
            tracing::warn!(error = %e, "LLM client unavailable for this command");
            None
        }
        Err(e) => return Err(e),
    };

    // ---------------- argv dispatch (one-shot command + exit) ----------------

    // `Tui` falls through to the chat-loop boot below instead of running
    // through `dispatch::run` — it's an interactive session, not a one-shot.
    if let Some(cmd) = cli.command.filter(|c| !matches!(c, Commands::Tui)) {
        let mut builder = ContextBuilder::new(Arc::clone(&config))
            .config_path(resolve_config_path())
            .skills(Arc::clone(&skill_registry))
            .tools(Arc::clone(&tool_registry))
            .channels(Arc::clone(&channels_registry))
            .workspace(Arc::clone(&workspace));
        if let Some(ref client) = llm_client {
            builder = builder.llm(Arc::clone(client));
        }
        let ctx = builder
            .build()
            .with_format(cli_format)
            .with_invocation(Invocation::Argv);

        match dispatch::run(&ctx, cmd).await {
            Ok(out) => {
                let rendered = out.render(cli_format);
                if !rendered.is_empty() {
                    println!("{rendered}");
                }
                return Ok(());
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }

    // ---------------- chat-loop boot (no subcommand provided) ----------------

    // Per-workspace singleton: the chat loop owns libsql, the job recovery
    // pass, and cron ticks — two instances against the same workspace would
    // race. Held for the lifetime of `main`; released by `Drop` on exit.
    let _workspace_lock = singleton::acquire(workspace.root.as_path())?;

    // By construction: the chat loop is only reached when `cli.command` is
    // `None`, and in that branch `build_llm_client` must have succeeded or we
    // already returned an error. Unwrap defensively to avoid a panic.
    let llm_client =
        llm_client.ok_or_else(|| anyhow::anyhow!("LLM client is required for chat loop"))?;

    // Storage layer — persistent libsql under the project root (`workspace.path`).
    let storage = Store::open(boot::storage_db_path(&config.workspace)).await?;

    // Skill risk assessor — LLM-backed classifier with hash-keyed cache.
    // Consulted lazily at skill-use time (slash dispatch + agent_loop gating).
    // Large skills fall into a tiered flow where `SKILL.md` is judged
    // synchronously and the full directory is handed to a background
    // worker; persisted job rows are recovered below so an interrupted
    // assessment resumes after restart instead of losing progress.
    let risk_store: Arc<dyn aura_storage::SkillRiskStore> = Arc::from(storage.risk);
    let skill_assessor = Arc::new(aura_skills_assessor::SkillAssessor::with_background_worker(
        Arc::clone(&llm_client),
        Arc::clone(&risk_store),
    ));
    {
        let registry = Arc::clone(&skill_registry);
        let lookup = move |name: &str| registry.get(name).map(|s| (*s).clone());
        match skill_assessor.recover_pending_jobs(lookup).await {
            Ok(0) => {}
            Ok(n) => info!(count = n, "re-enqueued skill-risk jobs from prior run"),
            Err(e) => tracing::warn!(error = %e, "failed to recover skill-risk jobs"),
        }
    }

    // Session manager
    let session_manager = Arc::new(SessionManager::new(
        storage.session,
        boot::to_session_timeout(&config.session),
    ));

    // Job manager — recover any jobs interrupted by a prior shutdown
    let job_manager = JobManager::new(storage.job);
    match job_manager.recover_interrupted().await {
        Ok(0) => {}
        Ok(n) => info!(count = n, "recovered interrupted jobs from prior run"),
        Err(e) => tracing::warn!(error = %e, "failed to recover interrupted jobs"),
    }
    let job_manager = Arc::new(job_manager);

    // Cost tracker
    let cost_tracker = Arc::new(CostTracker::new(storage.cost));

    // Trace collector
    let trace_store: Arc<dyn aura_storage::TraceStore> = Arc::from(storage.trace);
    let trace_collector = Arc::new(Mutex::new(TraceCollector::new(
        "global",
        Arc::clone(&trace_store),
        config.trace.auto_snapshot,
        config.trace.snapshot_interval,
    )));

    // Observability recorder
    let recorder = Arc::new(ObservabilityRecorder::new(
        Arc::clone(&job_manager),
        trace_collector,
        cost_tracker,
    ));

    // Secret vault
    let master_key = match boot::load_encryption_key(&config.security) {
        Ok(k) => k,
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
            EncryptionKey::new(b"aura-dev-master-key-32-bytes-ok!".to_vec())?
        }
    };
    let secret_vault = Arc::new(SecretVault::new(master_key, Arc::from(storage.secret)));

    // Tool executor
    let tool_executor = Arc::new(ToolExecutor::new(
        Arc::clone(&tool_registry),
        Arc::clone(&secret_vault),
        boot::to_tool_timeout(&config.tools),
    ));

    // Memory manager (without embedder for Phase 1)
    let memory_manager = Arc::new(MemoryManager::without_embedder(storage.memory));

    // Context manager (sliding window). Tokenizer picks an encoding based
    // on the configured model; Anthropic and other non-OpenAI models fall
    // back to cl100k_base as a close approximation.
    let tokenizer: Arc<dyn Tokenizer> =
        Arc::new(TiktokenTokenizer::for_model(llm_client.model_id()));

    // Soul derived from workspace identity files
    let soul = Soul::from_workspace(&workspace)
        .await
        .unwrap_or_else(|_| Soul::custom("You are Aura, an intelligent assistant.".to_string()));

    // Execution policy
    let policy = boot::to_execution_policy(&config.agent);

    // Hook manager (empty by default; hooks are loaded from config in later phases)
    let hook_manager = Arc::new(HookManager::new());

    // Message channels
    let (incoming_tx, incoming_rx) = mpsc::channel(buffer);
    let (response_tx, response_rx) = mpsc::channel(buffer);

    // Security gateway. The leak detector is shared (`Arc`) so the slash
    // context can expose the same rule set to `aura security leaks check`.
    let leak_detector = Arc::new(boot::build_leak_detector(&config.security));
    let security_gateway = Arc::new(SecurityGateway::new(
        Arc::clone(&leak_detector),
        Arc::clone(&secret_vault),
    ));

    // Shutdown signal — needed by the cron scheduler below and by the signal
    // handler task spawned later.
    let shutdown = ShutdownSignal::new();

    // Cron scheduler. Constructed before the slash context so the `cron list`
    // / `cron run` commands can reach it; the background tick loop is spawned
    // further down, once `task_tracker` exists.
    let (cron_trigger_tx, cron_trigger_rx) = mpsc::channel(64);
    let cron_scheduler = Arc::new(CronScheduler::new(
        storage.cron,
        cron_trigger_tx,
        shutdown.clone(),
    ));

    // Build the slash-handler context once everything above is wired.
    let slash_ctx = Arc::new(
        ContextBuilder::new(Arc::clone(&config))
            .config_path(resolve_config_path())
            .skills(Arc::clone(&skill_registry))
            .tools(Arc::clone(&tool_registry))
            .channels(Arc::clone(&channels_registry))
            .llm(Arc::clone(&llm_client))
            .workspace(Arc::clone(&workspace))
            .session(Arc::clone(&session_manager))
            .job(Arc::clone(&job_manager))
            .cron(Arc::clone(&cron_scheduler))
            .memory(Arc::clone(&memory_manager))
            .trace(Arc::clone(&trace_store))
            .tool_executor(Arc::clone(&tool_executor))
            .recorder(Arc::clone(&recorder))
            .security(Arc::clone(&security_gateway))
            .leak_detector(Arc::clone(&leak_detector))
            .skill_assessor(Arc::clone(&skill_assessor))
            .build()
            .with_invocation(Invocation::Slash)
            .with_format(OutputFormat::Plain),
    );
    let slash_handler = Arc::new(CliSlashHandler::new(Arc::clone(&slash_ctx)));
    let dashboard_provider = Arc::new(CliDashboardProvider::new(Arc::clone(&slash_ctx)));

    // Register and start the TUI adapter with slash + dashboard wiring
    // attached. Wire the log sink so warn/error tracing events echo into the
    // chat scrollback without corrupting raw-mode output.
    {
        let tui_shutdown = shutdown.clone();
        let tui = TuiAdapter::new()
            .with_slash_handler(slash_handler)
            .with_dashboard_provider(dashboard_provider)
            .with_on_exit(Arc::new(move || tui_shutdown.trigger()));
        if let Some(tracing) = chat_tracing.as_ref() {
            let _ = tracing.tui_sink.set(tui.log_sink());
        }
        let mut reg = channels_registry.write().await;
        reg.register(Box::new(tui))?;
        reg.start_all(incoming_tx).await?;
    }

    // Supervisor and Router
    let supervisor = AgentSupervisor::new(response_tx);
    let actor_llm_client = Arc::clone(&llm_client);
    let actor_tool_registry = Arc::clone(&tool_registry);
    let actor_skill_registry = Arc::clone(&skill_registry);
    let actor_tool_executor = Arc::clone(&tool_executor);
    let actor_memory_manager = Arc::clone(&memory_manager);
    let actor_policy = policy.clone();
    let actor_system_prompt = soul.system_prompt().to_string();
    let actor_tokenizer = Arc::clone(&tokenizer);
    let actor_hooks = Arc::clone(&hook_manager);
    let actor_recorder = Arc::clone(&recorder);
    let actor_skill_assessor = Arc::clone(&skill_assessor);
    let actor_token_budget = boot::to_token_budget(&config.agent.context);
    let actor_keep_recent = config.agent.context.keep_recent;

    let router = Router::new(
        Arc::clone(&session_manager),
        supervisor,
        Arc::clone(&channels_registry),
        security_gateway,
    )
    .with_actor_spawner(Box::new(move |session, response_tx| {
        let agent_loop = AgentLoop::new(
            Arc::clone(&actor_llm_client),
            Arc::clone(&actor_tool_registry),
            Arc::clone(&actor_skill_registry),
            Arc::clone(&actor_tool_executor),
            ContextManager::new(
                Arc::clone(&actor_tokenizer),
                Box::new(Truncate::new(actor_keep_recent)),
                actor_token_budget.clone(),
            ),
            Arc::clone(&actor_memory_manager),
            actor_policy.clone(),
            Soul::custom(actor_system_prompt.clone()),
        )
        .with_skill_assessor(Arc::clone(&actor_skill_assessor));
        let actor = AgentActor::new(
            session,
            agent_loop,
            response_tx,
            Arc::clone(&actor_hooks),
            Arc::clone(&actor_recorder),
        );
        let (sender, mailbox) = mpsc::channel(buffer);
        tokio::spawn(async move {
            actor.run(mailbox).await;
        });
        sender
    }));

    // Shutdown coordination. `shutdown` itself was created earlier so the
    // CronScheduler could take a clone; we still need the task tracker here.
    let mut task_tracker = TaskTracker::new();

    // Signal handler
    let sig_shutdown = shutdown.clone();
    task_tracker.track(tokio::spawn(async move {
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
        sig_shutdown.trigger();
    }));

    info!("all components initialized, starting router");

    // Spawn the cron scheduler's background tick loop (the scheduler itself
    // was built above alongside the slash context).
    let cron_handle = Arc::clone(&cron_scheduler);
    task_tracker.track(tokio::spawn(async move {
        cron_handle.run().await;
    }));

    let router = router.with_cron_triggers(cron_trigger_rx);

    // Run router with shutdown awareness
    // The CLI adapter's background task holds a clone of the incoming sender.
    // When the user types /quit or the adapter is stopped, the sender is
    // dropped and the router's incoming channel closes naturally.
    let router_shutdown = shutdown.clone();
    tokio::select! {
        _ = router.run(incoming_rx, response_rx) => {}
        _ = router_shutdown.wait() => {
            info!("shutdown signal received, stopping router");
        }
    }

    // Cleanup
    task_tracker.shutdown().await;
    info!("Aura shutdown complete");
    Ok(())
}

fn pick_format(cli: &Cli) -> OutputFormat {
    if cli.global.json {
        OutputFormat::Json
    } else if cli.global.plain {
        OutputFormat::Plain
    } else {
        OutputFormat::Human
    }
}

/// Emit a shell completion script without running the rest of the boot chain.
fn print_completion(shell: ShellKind) -> anyhow::Result<()> {
    let out = aura_cli::completion_script(shell).map_err(|e| anyhow::anyhow!(e))?;
    let rendered = out.render(OutputFormat::Plain);
    print!("{rendered}");
    Ok(())
}
