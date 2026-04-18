//! CLI entrypoint for the interactive chat loop (`aura tui`).
//!
//! Sibling of `gateway_cmd`: both long-lived modes go through the same
//! shape — build the manager graph via `runtime::build_managers` +
//! `wire_router`, register their own `ChannelAdapter`, and drive the
//! router + background tasks under a shared `ShutdownSignal`. `main.rs`
//! is only the dispatcher; each mode owns its own boot logic.

use std::path::PathBuf;
use std::sync::Arc;

use aura_agent::service::{ShutdownSignal, TaskTracker};
use aura_channels::TuiAdapter;
use aura_cli::{
    CliDashboardProvider, CliInputHistoryStore, CliSlashHandler, ContextBuilder, Invocation,
    OutputFormat,
};
use aura_config::AuraConfig;
use aura_security::LeakDetector;
use tracing::info;

use crate::runtime::{build_managers, force_exit_watchdog, install_signal_handler, wire_router};
use crate::{ChatTracing, resolve_config_path, singleton};

/// Run the interactive TUI to completion. Returns once the router
/// exits (user typed `/quit`, adapter closed) or the shared shutdown
/// signal fires (SIGINT/SIGTERM).
pub async fn run(
    config: Arc<AuraConfig>,
    leak_detector: Arc<LeakDetector>,
    chat_tracing: Option<ChatTracing>,
) -> anyhow::Result<()> {
    let workspace_root = PathBuf::from(&config.workspace.path);

    // Per-workspace singleton: the chat loop owns libsql, the job
    // recovery pass, and cron ticks — two instances against the same
    // workspace would race. Held for the lifetime of this call;
    // released by `Drop` on exit.
    let _workspace_lock = singleton::acquire(workspace_root.as_path())?;

    let shutdown = ShutdownSignal::new();
    let mut graph =
        build_managers(Arc::clone(&config), shutdown.clone(), leak_detector).await?;
    let run_handle = wire_router(&mut graph).await;

    // Slash-command context is assembled from the live graph so every
    // `/command` sees exactly the same manager instances the actor does.
    let slash_ctx = Arc::new(
        ContextBuilder::new(Arc::clone(&config))
            .config_path(resolve_config_path())
            .skills(Arc::clone(&graph.skill_registry))
            .tools(Arc::clone(&graph.tool_registry))
            .channels(Arc::clone(&graph.channels_registry))
            .llm(Arc::clone(&graph.llm_client))
            .workspace(Arc::clone(&graph.workspace))
            .session(Arc::clone(&graph.session_manager))
            .job(Arc::clone(&graph.job_manager))
            .cron(Arc::clone(&graph.cron_scheduler))
            .memory(Arc::clone(&graph.memory_manager))
            .trace(Arc::clone(&graph.trace_store))
            .security(Arc::clone(&graph.security_gateway))
            .leak_detector(Arc::clone(&graph.leak_detector))
            .skill_assessor(Arc::clone(&graph.skill_assessor))
            .build()
            .with_invocation(Invocation::Slash)
            .with_format(OutputFormat::Plain),
    );
    let slash_handler = Arc::new(CliSlashHandler::new(Arc::clone(&slash_ctx)));
    let dashboard_provider = Arc::new(CliDashboardProvider::new(Arc::clone(&slash_ctx)));

    // Build, register, and start the TUI adapter. The approval gate is
    // extracted automatically by `ChannelRegistry::register` and is
    // already visible to `ToolExecutor` through the shared
    // `ApprovalGateMap`.
    {
        let tui_shutdown = shutdown.clone();
        let history_store =
            Arc::new(CliInputHistoryStore::new(Arc::clone(&graph.secret_vault)));
        let tui = TuiAdapter::new()
            .with_slash_handler(slash_handler)
            .with_dashboard_provider(dashboard_provider)
            .with_input_history(history_store)
            .with_on_exit(Arc::new(move || tui_shutdown.trigger()));
        if let Some(tracing) = chat_tracing.as_ref() {
            let _ = tracing.tui_sink.set(tui.log_sink());
        }
        let mut reg = graph.channels_registry.write().await;
        reg.register(Box::new(tui))?;
        reg.start_all(run_handle.incoming_tx.clone()).await?;
    }

    let mut task_tracker = TaskTracker::new();
    install_signal_handler(&mut task_tracker, shutdown.clone());

    info!("all components initialized, starting router");

    let cron_handle = Arc::clone(&graph.cron_scheduler);
    task_tracker.track(tokio::spawn(async move {
        cron_handle.run().await;
    }));

    // The TUI adapter's background task holds a clone of the incoming
    // sender; `/quit` or an adapter stop drops the sender and the
    // router's incoming channel closes naturally.
    let router_shutdown = shutdown.clone();
    tokio::select! {
        _ = run_handle.router.run(run_handle.incoming_rx, run_handle.response_rx) => {}
        _ = router_shutdown.wait() => {
            info!("shutdown signal received, stopping router");
        }
    }

    // A tool still running when the TUI quits can stall graceful
    // teardown. An OS thread outside the tokio runtime is immune and
    // force-exits if the budget is exceeded.
    force_exit_watchdog(std::time::Duration::from_secs(10));

    task_tracker.shutdown().await;
    info!("Aura shutdown complete");
    Ok(())
}
