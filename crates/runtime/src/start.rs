//! Reusable gateway start path — the orchestration that `aura gateway start`
//! used to inline, extracted so both the CLI and the macOS app (which embeds
//! the runtime in-process) stand up an identical manager graph + gateway.
//! See `docs/mac-app.md` §3.
//!
//! The host stays in control of process-global concerns: tracing install is a
//! caller-supplied closure (so its redaction can cover the freshly-minted
//! tokens), and signal handling / force-exit / the channel TCP listener are
//! opt-in flags. The CLI turns them all on; the embed leaves them off and
//! lets Tauri own the process.

use std::sync::Arc;
use std::time::Duration;

use aura_agent::service::{ShutdownSignal, TaskTracker};
use aura_config::AuraConfig;
use aura_gateway::{
    AdminToken, BoundGateway, ChannelServer, ChannelSpawner, ChannelTokenTable, ClientIdentity,
    GatewayDeps, GatewayServer, RuntimeGatewayConfig, SidecarSupervisor, TUI_CLIENT_LABEL,
    TUI_TOKEN_VAULT_KEY,
};
use aura_security::LeakDetector;
use aura_workspace::WorkspacePaths;

use crate::runtime::RouterRunHandle;
use crate::singleton::WorkspaceLock;
use crate::{boot, runtime};

/// Opaque keep-alive for the host's tracing guards (e.g. the file-appender
/// `WorkerGuard`). Held for the runtime's lifetime so the appender keeps
/// flushing; dropped when [`RunningGateway`] is dropped.
pub type TracingGuard = Box<dyn std::any::Any + Send>;

/// Build the host's tracing and hand back the [`LogBuffer`] `GatewayDeps`
/// needs. Called after the leak detector is built (seeded with the minted
/// admin + TUI tokens) and before the manager graph, so a host that installs
/// a global subscriber gets token redaction for free. The returned
/// [`TracingGuard`] is stored on [`RunningGateway`] to keep any appender guard
/// alive.
pub type SetupTracing = Box<
    dyn FnOnce(&WorkspacePaths, Arc<LeakDetector>) -> (Arc<aura_gateway::LogBuffer>, TracingGuard)
        + Send,
>;

/// Inputs to [`start_gateway`]. `config.workspace.path` is authoritative for
/// the workspace root — the embed sets it to its app-owned data dir before
/// calling (see `docs/mac-app.md` §4); the CLI passes its loaded config as-is.
pub struct StartGatewayOpts {
    pub config: Arc<AuraConfig>,
    /// Explicit path to the on-disk `aura.json` for the reload surface
    /// (`GET /v1/config` + SIGHUP). The embed passes its app-owned path so it
    /// never falls back to the `~/.aura` default; the CLI passes `None` to keep
    /// the env/default resolution it has always used.
    pub config_path: Option<std::path::PathBuf>,
    /// Install the SIGINT/SIGTERM handler + SIGHUP config-reload loop, and run
    /// the force-exit watchdog on shutdown. CLI: `true`. Embed: `false`
    /// (Tauri owns signals + the process).
    pub install_signals: bool,
    /// Bind the separate loopback `ChannelServer` (for the TUI + channel
    /// sidecars) and run the channel-sidecar supervisor. CLI: `true`. Embed:
    /// `false` — the admin bind co-hosts the channel-ws subrouter, so the
    /// webview never needs the second listener.
    pub start_channel_listener: bool,
    /// Host-owned tracing setup; see [`SetupTracing`].
    pub setup_tracing: SetupTracing,
}

/// A fully wired, bound gateway. The admin TCP listener is already bound (so
/// [`Self::admin_addr`] is the real OS-assigned port), background loops
/// (cron / janitor / reconcilers) are already spawned, and the caller drives
/// the servers to completion via [`Self::serve`] — the CLI awaits it
/// (blocking until shutdown); the embed spawns it and keeps the handle.
pub struct RunningGateway {
    pub admin_addr: std::net::SocketAddr,
    pub admin_token: String,
    pub shutdown: ShutdownSignal,
    pub shutdown_grace: Duration,
    /// Held for the runtime's lifetime; releases `state/aura.lock` on drop.
    _workspace_lock: WorkspaceLock,
    _tracing_guard: TracingGuard,
    force_exit_on_shutdown: bool,
    bound_admin: BoundGateway,
    channel_server: Option<ChannelServer>,
    run_handle: RouterRunHandle,
    task_tracker: TaskTracker,
}

impl RunningGateway {
    /// Drive the admin server, the optional channel server, and the router to
    /// completion, returning once [`ShutdownSignal`] fires and in-flight work
    /// drains. Equivalent to the old `gateway start` `select!` body.
    pub async fn serve(self) -> anyhow::Result<()> {
        let RunningGateway {
            shutdown,
            shutdown_grace,
            force_exit_on_shutdown,
            bound_admin,
            channel_server,
            run_handle,
            task_tracker,
            ..
        } = self;

        let admin_shutdown = shutdown.clone();
        let channel_shutdown = shutdown.clone();
        let router_shutdown = shutdown.clone();

        // The optional channel listener: when absent, a pending future so the
        // `select!` arm never resolves on its own.
        let channel_fut = async move {
            match channel_server {
                Some(cs) => cs.run(channel_shutdown).await,
                None => {
                    std::future::pending::<()>().await;
                    Ok(())
                }
            }
        };

        tokio::select! {
            res = bound_admin.serve(admin_shutdown) => {
                if let Err(e) = res {
                    tracing::error!(error = %e, "admin gateway server exited with error");
                    shutdown.trigger();
                    return Err(anyhow::anyhow!("gateway server error: {e}"));
                }
            }
            res = channel_fut => {
                if let Err(e) = res {
                    tracing::error!(error = %e, "channel gateway server exited with error");
                    shutdown.trigger();
                    return Err(anyhow::anyhow!("channel server error: {e}"));
                }
            }
            _ = run_handle.router.run(run_handle.incoming_rx, run_handle.response_rx) => {
                tracing::info!("router exited before server; triggering shutdown");
                shutdown.trigger();
            }
            _ = router_shutdown.wait() => {
                tracing::info!("shutdown signal received, stopping gateway");
            }
        }

        if force_exit_on_shutdown {
            runtime::force_exit_watchdog(shutdown_grace);
        }
        task_tracker.shutdown().await;
        tracing::info!("gateway shutdown complete");
        Ok(())
    }
}

/// Stand up the manager graph + gateway from `opts`. Mints (or reads) the
/// admin token, seeds the leak detector with it, builds the full actor graph,
/// binds the admin listener, spawns the background loops, and returns a
/// [`RunningGateway`] the caller drives via [`RunningGateway::serve`].
pub async fn start_gateway(opts: StartGatewayOpts) -> anyhow::Result<RunningGateway> {
    let StartGatewayOpts {
        config,
        config_path,
        install_signals,
        start_channel_listener,
        setup_tracing,
    } = opts;

    // Per-workspace singleton. The gateway owns the same libsql store as the
    // TUI, runs job recovery, and drives cron ticks — two instances against
    // the same workspace would race.
    let workspace_paths = WorkspacePaths::new(std::path::PathBuf::from(&config.workspace.path));
    aura_workspace::WorkspaceManager::new(workspace_paths.root().to_path_buf())
        .ensure_layout()
        .await?;
    let workspace_lock = crate::singleton::acquire(workspace_paths.root())?;

    // Register SIGHUP before the long boot work so a concurrent `aura llm`
    // edit that signals our freshly-recorded pid can't kill us mid-boot.
    let sighup = if install_signals {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::hangup()) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::error!(error = %e, "failed to register SIGHUP; config reload-on-signal disabled");
                None
            }
        }
    } else {
        None
    };

    // Read the admin token AND mint+publish a fresh TUI token BEFORE building
    // the manager graph: both are registered as `LeakAction::Replace` rules on
    // the LeakDetector inside `build_managers`. The admin token is auto-minted
    // on first run.
    let (token, tui_token) = {
        let vault = runtime::build_secret_vault(&config).await?;
        let admin_token = AdminToken::new(Arc::clone(&vault)).mint_if_absent().await?;
        let tui_token = aura_gateway::generate_token();
        vault
            .store_secret(TUI_TOKEN_VAULT_KEY, tui_token.as_bytes())
            .await
            .map_err(|e| anyhow::anyhow!("publish TUI token to vault: {e}"))?;
        (admin_token, tui_token)
    };

    // Build the leak detector (with every gateway-minted token registered as a
    // `LeakAction::Replace` rule) BEFORE the host installs tracing so log lines
    // that echo a credential are masked. The same `Arc<LeakDetector>` feeds
    // `build_managers` so the runtime graph's SecurityGateway shares the rules.
    let leak_detector = runtime::build_leak_detector(
        &config.security,
        &[
            ("gateway.admin_token", token.as_str()),
            (TUI_TOKEN_VAULT_KEY, tui_token.as_str()),
        ],
    );
    let (log_buffer, tracing_guard) = setup_tracing(&workspace_paths, Arc::clone(&leak_detector));
    tracing::info!(token_len = token.len(), "gateway token loaded from vault");
    tracing::info!(token_len = tui_token.len(), "fresh TUI token published to vault");

    // Resolve the runtime gateway config up front so a bad `bind_address` fails
    // fast before we open libsql a second time.
    let runtime_cfg = RuntimeGatewayConfig::from_config(&config.gateway)
        .map_err(|e| anyhow::anyhow!("invalid gateway config: {e}"))?;

    // Channel-token table for the TUI handshake.
    let channel_tokens = ChannelTokenTable::new();
    let _tui_token_handle = channel_tokens.register(
        tui_token.clone(),
        ClientIdentity {
            pid: std::process::id(),
            label: TUI_CLIENT_LABEL.to_string(),
            bound_channel_type: None,
        },
    );

    let port_file = workspace_paths.channel_port();

    // Install the embedded sidecar runtime once and reuse it for both the
    // MCP-profile collection (browser MCP server) and the channel-sidecar
    // supervisor.
    let sidecar_runtime: Option<Arc<aura_gateway::SidecarRuntime>> =
        match aura_gateway::SidecarRuntime::install() {
            Ok(rt) => Some(Arc::new(rt)),
            Err(e) => {
                tracing::info!(
                    error = %e,
                    "embedded sidecar runtime unavailable; no embedded sidecars will be spawned",
                );
                None
            }
        };
    let embedded_mcp_servers: Vec<aura_tools::mcp::EmbeddedMcpServer> = sidecar_runtime
        .as_deref()
        .map(|rt| {
            aura_tools::mcp::embedded_servers(&aura_gateway::collect_profiles(
                rt,
                &config,
                &workspace_paths,
            ))
        })
        .unwrap_or_default();

    let shutdown = ShutdownSignal::new();
    // Resolved config path, or the default path when none existed at boot — so
    // a first-run `aura llm add` that creates the file and SIGHUPs us still
    // hot-reloads. Feeds both the reloader and the admin read/mutate surface.
    let reload_config_path = config_path
        .or_else(boot::resolve_config_path)
        .or_else(|| Some(aura_workspace::paths::default_config_file()));
    let mut graph = runtime::build_managers(
        Arc::clone(&config),
        reload_config_path.clone(),
        shutdown.clone(),
        Arc::clone(&leak_detector),
        embedded_mcp_servers,
    )
    .await?;
    let run_handle = runtime::wire_router(&mut graph).await;

    let mut task_tracker = TaskTracker::new();
    if install_signals {
        runtime::install_signal_handler(&mut task_tracker, shutdown.clone());
    }

    // SIGHUP → config hot-reload, draining the stream registered before boot.
    if let Some(mut hup) = sighup {
        let reloader = Arc::clone(&graph.config_reloader);
        let hup_shutdown = shutdown.clone();
        task_tracker.track(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = hup.recv() => match reloader.reload().await {
                        Ok(o) => {
                            tracing::info!(active_model = %o.active_model, "SIGHUP: config reloaded")
                        }
                        Err(e) => tracing::warn!(error = %e, "SIGHUP: config reload failed"),
                    },
                    _ = hup_shutdown.wait() => break,
                }
            }
        }));
    }

    // Cron tick loop.
    let cron_handle = Arc::clone(&graph.cron_scheduler);
    task_tracker.track(tokio::spawn(async move {
        cron_handle.run().await;
    }));

    {
        let mut janitor = aura_janitor::Janitor::new(workspace_paths.clone())
            .with_pairing_store(graph.stores.channel_pairing.clone());
        if let Some(runtime) = sidecar_runtime.as_ref()
            && let Some(cache_root) = runtime.sidecars_cache_root()
        {
            janitor = janitor.with_sidecar_cache(aura_janitor::SidecarCache {
                cache_root,
                live_dirs: runtime.live_dir_names(),
            });
        }
        let janitor_shutdown = shutdown.clone();
        task_tracker.track(tokio::spawn(async move {
            janitor
                .run(async move { janitor_shutdown.wait().await })
                .await;
        }));
    }

    let channel_control = Arc::new(aura_gateway::ChannelControlRegistry::new());

    // CLI-driven bot add/remove writes straight to libsql + vault; the
    // reconciler polls those stores and pushes StartBot/StopBot frames.
    let bot_reconciler = Arc::new(aura_gateway::channel::ChannelBotReconciler::new(
        Arc::clone(&channel_control),
        graph.stores.channel_bot.clone(),
        Arc::clone(&graph.secret_vault),
    ));
    {
        let reconciler = Arc::clone(&bot_reconciler);
        let shutdown_for_reconciler = shutdown.clone();
        task_tracker.track(tokio::spawn(async move {
            reconciler.run(shutdown_for_reconciler).await;
        }));
    }

    // Stash for web-chat token handles, shared between the admin chat mint side
    // and the channel WS take side.
    let web_chat_tokens = Arc::new(Default::default());

    // TTL reaper for web_chat_tokens — drops handles a WS upgrade never claimed.
    {
        let janitor = aura_gateway::channel::WebTokenJanitor::new(Arc::clone(&web_chat_tokens));
        let shutdown_for_janitor = shutdown.clone();
        task_tracker.track(tokio::spawn(async move {
            janitor.run(shutdown_for_janitor).await;
        }));
    }

    let deps = GatewayDeps {
        config: Arc::clone(&graph.config),
        config_path: reload_config_path,
        runtime_config: runtime_cfg.clone(),
        session_manager: Arc::clone(&graph.session_manager),
        job_lifecycle: Arc::clone(&graph.job_lifecycle),
        cron_scheduler: Arc::clone(&graph.cron_scheduler),
        skill_registry: Arc::clone(&graph.skill_registry),
        tool_registry: Arc::clone(&graph.tool_registry),
        channel_registry: Arc::clone(&graph.channels_registry),
        llm_pool: Arc::clone(&graph.llm_pool),
        supervisor: run_handle.supervisor.clone(),
        config_reloader: Arc::clone(&graph.config_reloader),
        admin_token: token.clone(),
        log_buffer: Arc::clone(&log_buffer),
        incoming_tx: run_handle.incoming_tx.clone(),
        channel_tokens: channel_tokens.clone(),
        web_chat_tokens,
        secret_vault: Arc::clone(&graph.secret_vault),
        stores: graph.stores.clone(),
        channel_control,
        bot_reconciler: Arc::clone(&bot_reconciler),
    };

    // Optional loopback channel listener (TUI + channel sidecars). Off in the
    // embed — the admin bind co-hosts the channel-ws subrouter.
    let channel_server = if start_channel_listener {
        let cs = ChannelServer::bind(&deps, port_file, channel_tokens.clone())
            .map_err(|e| anyhow::anyhow!("bind channel TCP listener: {e}"))?;
        let channel_port = cs.port();
        let channel_url = format!("ws://127.0.0.1:{channel_port}/v1/channel-ws");

        // Channel-sidecar supervisor (telegram / weixin / …).
        if let Some(runtime) = sidecar_runtime.as_ref() {
            let domains: Vec<&str> = runtime.domains().collect();
            if domains.is_empty() {
                tracing::info!("no embedded sidecars in this build");
            } else {
                for domain in &domains {
                    let names: Vec<&str> = runtime.names_in_domain(domain).collect();
                    tracing::info!(
                        domain = %domain,
                        sidecars = ?names,
                        channel_port,
                        "sidecar runtime materialised",
                    );
                }
            }

            let channel_only: Vec<String> = runtime
                .names_in_domain(aura_gateway::sidecar::domains::CHANNEL)
                .map(String::from)
                .collect();
            if !channel_only.is_empty() {
                let spawner = ChannelSpawner::new(
                    channel_url.clone(),
                    channel_tokens.clone(),
                    boot::proxy_settings(&config),
                );
                let supervisor = SidecarSupervisor::new(
                    Arc::clone(runtime),
                    spawner,
                    Arc::clone(&log_buffer),
                    workspace_paths.channel_logs_dir(),
                    Arc::clone(&leak_detector),
                    graph.stores.channel_bot.clone(),
                );
                let sv_shutdown = shutdown.clone();
                task_tracker.track(tokio::spawn(async move {
                    supervisor.run(sv_shutdown).await;
                }));
            }
        }
        Some(cs)
    } else {
        None
    };

    let bound_admin = GatewayServer::new(deps).bind_listener().await?;
    let admin_addr = bound_admin.local_addr();
    tracing::info!(bind = %admin_addr, "gateway start: all components initialized");

    Ok(RunningGateway {
        admin_addr,
        admin_token: token,
        shutdown,
        shutdown_grace: runtime_cfg.shutdown_grace,
        _workspace_lock: workspace_lock,
        _tracing_guard: tracing_guard,
        force_exit_on_shutdown: install_signals,
        bound_admin,
        channel_server,
        run_handle,
        task_tracker,
    })
}
