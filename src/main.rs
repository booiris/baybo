mod boot;

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
use aura_channels::{ChannelRegistry, CliAdapter};
use aura_cli::cli::ShellKind;
use aura_cli::{
    Cli, CliSlashHandler, Commands, ContextBuilder, Invocation, OutputFormat, dispatch,
};
use aura_context::{ContextManager, TiktokenTokenizer, Tokenizer, Truncate};
use aura_hook::HookManager;
use aura_security::EncryptionKey;
use aura_skills::SkillRegistry;
use aura_storage::Store;
use aura_tools::ToolRegistry;
use aura_workspace::WorkspaceManager;
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, mpsc};
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

fn init_tracing() {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("aura=info"));

    let log_format = std::env::var("AURA_LOG_FORMAT").unwrap_or_default();

    if log_format == "json" {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt::layer().json().with_target(true).with_span_list(true))
            .init();
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt::layer().with_target(true))
            .init();
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

    init_tracing();

    let cli_format = pick_format(&cli);

    info!("Aura - Intelligent Assistant Framework starting");

    let config = boot::load_config().await?;
    let config = Arc::new(config);
    let buffer = config.channels.message_buffer_size;

    // --- minimal services required by both argv and chat modes ---

    let skill_registry = Arc::new(SkillRegistry::new());
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

    if let Some(cmd) = cli.command {
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

    // By construction: the chat loop is only reached when `cli.command` is
    // `None`, and in that branch `build_llm_client` must have succeeded or we
    // already returned an error. Unwrap defensively to avoid a panic.
    let llm_client =
        llm_client.ok_or_else(|| anyhow::anyhow!("LLM client is required for chat loop"))?;

    // Storage layer — persistent libsql under the project root (`workspace.path`).
    let storage = Store::open(boot::storage_db_path(&config.workspace)).await?;

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
            .build()
            .with_invocation(Invocation::Slash)
            .with_format(OutputFormat::Plain),
    );
    let slash_handler = Arc::new(CliSlashHandler::new(slash_ctx));

    // Register and start the CLI adapter with the slash handler attached.
    {
        let mut reg = channels_registry.write().await;
        reg.register(Box::new(
            CliAdapter::new().with_slash_handler(slash_handler),
        ))
        .expect("failed to register CLI adapter");
        reg.start_all(incoming_tx)
            .await
            .expect("failed to start channels");
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
        );
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
