//! CLI entrypoint for `baybo gateway …` subcommands.
//!
//! Intercepted from `main.rs` before the normal argv dispatch path
//! (same pattern as `Commands::Tui`). The non-serving subcommands
//! (`install`, `disable`, `uninstall`, `status`, `enable`, `token`) run
//! here with a lightweight boot: they need only the config for path
//! resolution, plus — for the auth-token branch — a `SecretVault`
//! opened against the project's sqlite store.
//!
//! `start` is a long-running server: it acquires the per-workspace
//! singleton, builds the full manager graph via [`crate::runtime`]
//! (seeding `runtime::build_leak_detector` with the gateway-owned
//! tokens so any log line that echoes them is masked), and drives the
//! router, admin [`GatewayServer`], and the loopback-TCP
//! [`ChannelServer`] side by side under a shared `ShutdownSignal`.
//! Sidecars register themselves with the channel registry from the WS
//! route task when they connect.

use std::path::PathBuf;
use std::sync::Arc;

use baybo_agent::service::{ShutdownSignal, TaskTracker};
use baybo_cli::cli::{GatewayCmd, GatewayTokenCmd};
use baybo_config::BayboConfig;
use baybo_gateway::installer::{self, InstallContext, ServiceInstaller};
use baybo_gateway::{
    AdminToken, ChannelServer, ChannelSpawner, ChannelTokenTable, ClientIdentity, GatewayDeps,
    GatewayServer, RuntimeGatewayConfig, SidecarSupervisor, TUI_CLIENT_LABEL, TUI_TOKEN_VAULT_KEY,
};

use crate::boot;
use crate::runtime;
use crate::tracing_init::{TracingMode, init_tracing};

/// Entry point — routes the parsed subcommand to the right handler.
pub async fn run(cmd: GatewayCmd) -> anyhow::Result<()> {
    let config = boot::load_config().await?;
    let config = Arc::new(config);

    match cmd {
        GatewayCmd::Start => start(config).await,
        GatewayCmd::Install { system, exec_start } => {
            install_service(&config, system, exec_start.map(PathBuf::from))
        }
        GatewayCmd::Enable => enable(&config).await,
        GatewayCmd::Disable => disable(&config),
        GatewayCmd::Uninstall { yes: _ } => uninstall(&config).await,
        GatewayCmd::Status => status(&config),
        GatewayCmd::Token { cmd } => match cmd {
            GatewayTokenCmd::Show => token_show(&config).await,
            GatewayTokenCmd::Rotate { yes: _ } => token_rotate(&config).await,
        },
    }
}

// ---- installer subcommands (no vault needed) ----

fn make_installer(user_mode: bool) -> anyhow::Result<Box<dyn ServiceInstaller>> {
    installer::for_current_platform(user_mode)
        .map_err(|e| anyhow::anyhow!("no installer for this platform: {e}"))
}

fn install_context(
    config: &BayboConfig,
    explicit_exec: Option<PathBuf>,
) -> anyhow::Result<InstallContext> {
    let exec_start = installer::resolve_exec_start(explicit_exec.as_deref())
        .map_err(|e| anyhow::anyhow!("cannot resolve executable path: {e}"))?;
    let config_path = resolve_install_config_path()?;
    let log_dir =
        baybo_workspace::WorkspacePaths::new(PathBuf::from(&config.workspace.path)).logs_dir();
    Ok(InstallContext {
        exec_start,
        config_path,
        log_dir,
        user_mode: true,
    })
}

/// Pick the config path to bake into the unit file.
///
/// Resolution order:
/// 1. `BAYBO_CONFIG_PATH` — canonicalized; errors if it cannot be
///    resolved to an existing file.
/// 2. `<default_workspace_root>/config/baybo.json` — if it already exists.
/// 3. Otherwise `None`, with a loud stderr warning: the service will
///    launch against built-in defaults (no LLM key, default workspace),
///    which is almost never what the user actually wants.
///
/// systemd/launchd don't inherit the invoking shell's env, so the path
/// has to be baked into the unit file at install time — there's no
/// second chance to pick it up later.
fn resolve_install_config_path() -> anyhow::Result<Option<PathBuf>> {
    use baybo_workspace::paths::{ENV_CONFIG_PATH, default_config_file};

    if let Some(raw) = std::env::var_os(ENV_CONFIG_PATH) {
        let p = PathBuf::from(&raw);
        let canon = p.canonicalize().map_err(|e| {
            anyhow::anyhow!(
                "{ENV_CONFIG_PATH}={} cannot be resolved ({e}). Fix the path or unset the variable \
                 before running install — a broken path baked into the unit file would only \
                 surface later when the service fails to start.",
                p.display()
            )
        })?;
        return Ok(Some(canon));
    }

    let default_path = default_config_file();
    if default_path.exists() {
        let canon = default_path.canonicalize().map_err(|e| {
            anyhow::anyhow!(
                "{} exists but cannot be canonicalized: {e}",
                default_path.display()
            )
        })?;
        eprintln!(
            "install: no {ENV_CONFIG_PATH} set; pinning service to {}",
            canon.display()
        );
        return Ok(Some(canon));
    }

    eprintln!(
        "install: WARNING — no {ENV_CONFIG_PATH} and no {} on disk; the installed \
         service will run with built-in defaults (no LLM key, default workspace). Set \
         {ENV_CONFIG_PATH} and re-run `baybo gateway install` if that's not what you want.",
        default_path.display(),
    );
    Ok(None)
}

fn install_service(
    config: &BayboConfig,
    system: bool,
    explicit_exec: Option<PathBuf>,
) -> anyhow::Result<()> {
    let installer = make_installer(!system)?;
    let mut ctx = install_context(config, explicit_exec)?;
    ctx.user_mode = !system;
    let path = installer
        .install(&ctx)
        .map_err(|e| anyhow::anyhow!("install failed: {e}"))?;
    println!("installed {}", path.display());
    Ok(())
}

fn disable(_config: &BayboConfig) -> anyhow::Result<()> {
    let installer = make_installer(true)?;
    installer
        .disable()
        .map_err(|e| anyhow::anyhow!("disable failed: {e}"))?;
    println!("disabled");
    Ok(())
}

fn status(_config: &BayboConfig) -> anyhow::Result<()> {
    let installer = make_installer(true)?;
    let status = installer
        .status()
        .map_err(|e| anyhow::anyhow!("status lookup failed: {e}"))?;
    println!("{status:?}");
    Ok(())
}

async fn uninstall(_config: &BayboConfig) -> anyhow::Result<()> {
    let installer = make_installer(true)?;
    installer
        .uninstall()
        .map_err(|e| anyhow::anyhow!("uninstall failed: {e}"))?;
    println!("uninstalled");
    Ok(())
}

// ---- vault-backed subcommands ----

async fn enable(config: &BayboConfig) -> anyhow::Result<()> {
    let vault = runtime::build_secret_vault(config).await?;
    let token_mgr = AdminToken::new(vault);
    let token = token_mgr.mint_if_absent().await?;

    // Best-effort mark the unit enabled — not fatal if the service
    // isn't installed yet; `enable` is allowed before `install`.
    match make_installer(true) {
        Ok(installer) => {
            if let Err(e) = installer.enable() {
                tracing::warn!(error = %e, "unit not enabled; run `baybo gateway install` first");
            }
        }
        Err(e) => tracing::warn!(error = %e, "no installer available"),
    }

    println!("gateway enabled; token: {token}");
    Ok(())
}

async fn token_show(config: &BayboConfig) -> anyhow::Result<()> {
    let vault = runtime::build_secret_vault(config).await?;
    let token_mgr = AdminToken::new(vault);
    match token_mgr.get().await? {
        Some(t) => println!("{t}"),
        None => {
            return Err(anyhow::anyhow!(
                "no token minted; run `baybo gateway enable` first"
            ));
        }
    }
    Ok(())
}

async fn token_rotate(config: &BayboConfig) -> anyhow::Result<()> {
    let vault = runtime::build_secret_vault(config).await?;
    let token_mgr = AdminToken::new(vault);
    let token = token_mgr.rotate().await?;
    println!("{token}");
    Ok(())
}

// ---- start (long-running) ----

async fn start(config: Arc<BayboConfig>) -> anyhow::Result<()> {
    // Per-workspace singleton. The gateway owns the same sqlite store as
    // the TUI, runs turn recovery, and drives cron ticks — two instances
    // against the same workspace would race.
    let workspace_paths =
        baybo_workspace::WorkspacePaths::new(PathBuf::from(&config.workspace.path));
    baybo_workspace::WorkspaceManager::new(workspace_paths.root().to_path_buf())
        .ensure_layout()
        .await?;
    let _workspace_lock = baybo_workspace::acquire_workspace_lock(workspace_paths.root())?;

    // Register SIGHUP **before** the long boot work below (manager build,
    // sidecar install, router wiring). Until a signal stream exists SIGHUP
    // sits at its default disposition — terminate — so a concurrent `baybo
    // llm` edit that signals our freshly-recorded pid (singleton wrote it
    // just above) would kill the gateway mid-boot. Creating the stream now
    // flips the disposition and buffers any boot-time signal; the reload
    // loop drains it once the reloader exists.
    let sighup = {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::hangup()) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::error!(error = %e, "failed to register SIGHUP; config reload-on-signal disabled");
                None
            }
        }
    };

    // Read the admin token AND mint+publish a fresh TUI token BEFORE
    // building the manager graph: both are registered as
    // `LeakAction::Replace` rules on the LeakDetector, which happens
    // inside `build_managers` before the detector is sealed into an
    // Arc. The admin token is auto-minted on first run so a fresh
    // workspace can `baybo gateway start` without a prior `enable`. The
    // TUI token is rotated unconditionally on every start — semantics
    // of a "temporary" token: any TUI still holding the previous
    // generation's value must reconnect via the freshly published
    // vault entry.
    let (token, tui_token) = {
        let vault = runtime::build_secret_vault(&config).await?;
        let admin_token = AdminToken::new(Arc::clone(&vault)).mint_if_absent().await?;
        let tui_token = baybo_gateway::generate_token();
        vault
            .store_secret(TUI_TOKEN_VAULT_KEY, tui_token.as_bytes())
            .await
            .map_err(|e| anyhow::anyhow!("publish TUI token to vault: {e}"))?;
        (admin_token, tui_token)
    };

    // Build the leak detector (with every gateway-minted token
    // registered as a `LeakAction::Replace` rule) BEFORE initialising
    // tracing so log lines that accidentally echo any credential are
    // masked on disk. Pass the same `Arc<LeakDetector>` into
    // `build_managers` so the runtime graph's SecurityGateway uses
    // the same rule set.
    let leak_detector = runtime::build_leak_detector(
        &config.security,
        &[
            ("gateway.admin_token", token.as_str()),
            (TUI_TOKEN_VAULT_KEY, tui_token.as_str()),
        ],
    );
    let log_dir = workspace_paths.logs_dir();
    let tracing_guards = init_tracing(TracingMode::File {
        log_dir: &log_dir,
        leak_detector: Arc::clone(&leak_detector),
    });
    let log_buffer = tracing_guards.log_buffer();
    tracing::info!(token_len = token.len(), "gateway token loaded from vault");
    tracing::info!(
        token_len = tui_token.len(),
        "fresh TUI token published to vault"
    );

    // Resolve the runtime gateway config up front so a bad `bind_address`
    // fails fast before we open sqlite a second time.
    let runtime_cfg = RuntimeGatewayConfig::from_config(&config.gateway)
        .map_err(|e| anyhow::anyhow!("invalid gateway config: {e}"))?;

    // Channel-token table for the TUI handshake. The browser MCP
    // sidecar (chrome-devtools-mcp wrapper) does not use the blob
    // upload backchannel — its screenshots flow through the standard
    // MCP `attachImage` content type and are decoded by Baybo's
    // gateway-side `content_adapter`. So no tool/* token registration
    // is needed here.
    let channel_tokens = ChannelTokenTable::new();
    let _tui_token_handle = channel_tokens.register(
        tui_token.clone(),
        ClientIdentity {
            pid: std::process::id(),
            label: TUI_CLIENT_LABEL.to_string(),
            // None: TUI's channel-type binding is enforced via
            // `TUI_CLIENT_LABEL` in the handshake, not via this
            // field. Subprocess sidecars get `Some(channel_type)`
            // from `ChannelSpawner::spawn`.
            bound_channel_type: None,
        },
    );

    // Workspace channel-port file — the channel listener writes its
    // bound port here at start.
    let port_file = workspace_paths.channel_port();

    // Install the embedded sidecar runtime once and reuse it for both
    // the MCP-profile collection (browser MCP server) below and the
    // channel-sidecar supervisor further down. Idempotent install but
    // keeping a single Arc avoids the second filesystem walk.
    let sidecar_runtime: Option<Arc<baybo_gateway::SidecarRuntime>> =
        match baybo_gateway::SidecarRuntime::install() {
            Ok(rt) => Some(Arc::new(rt)),
            Err(e) => {
                tracing::info!(
                    error = %e,
                    "embedded sidecar runtime unavailable; no embedded sidecars will be spawned",
                );
                None
            }
        };
    let embedded_mcp_servers: Vec<baybo_tools::mcp::EmbeddedMcpServer> = sidecar_runtime
        .as_deref()
        .map(|rt| {
            baybo_tools::mcp::embedded_servers(&baybo_gateway::collect_profiles(
                rt,
                &config,
                &workspace_paths,
            ))
        })
        .unwrap_or_default();

    let shutdown = ShutdownSignal::new();
    // The resolved config path, or the default path when none existed at
    // boot — so a first-run `baybo llm add` that creates the file and
    // SIGHUPs us still hot-reloads instead of silently returning
    // `NoConfigPath`. This single value feeds both the reloader and the
    // admin read/mutate surface (`GatewayDeps.config_path`) so the two
    // never diverge — otherwise `GET /v1/config` would keep reporting the
    // boot snapshot after a first-run reload had already applied the file.
    let reload_config_path =
        boot::resolve_config_path().or_else(|| Some(baybo_workspace::paths::default_config_file()));
    let mut graph = runtime::build_managers(
        Arc::clone(&config),
        reload_config_path.clone(),
        shutdown.clone(),
        Arc::clone(&leak_detector),
        embedded_mcp_servers,
    )
    .await?;
    let run_handle = runtime::wire_router(&mut graph).await;

    // WS-backed sidecars register themselves from the route task when a
    // client connects; nothing to pre-register here.

    let mut task_tracker = TaskTracker::new();
    runtime::install_signal_handler(&mut task_tracker, shutdown.clone());

    // SIGHUP → config hot-reload, draining the stream registered before
    // boot. Covers hand-edits to baybo.json and the `baybo llm` CLI (which
    // signals after writing config and/or rotating a vault credential).
    // `reload` always rebuilds the LLM pool, so a vault key rotation —
    // invisible in the config diff — still takes effect. A signal that
    // arrived during boot was buffered by the stream and is processed here.
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
        let mut janitor = baybo_janitor::Janitor::new(workspace_paths.clone())
            .with_pairing_store(graph.stores.channel_pairing.clone());
        if let Some(runtime) = sidecar_runtime.as_ref()
            && let Some(cache_root) = runtime.sidecars_cache_root()
        {
            janitor = janitor.with_sidecar_cache(baybo_janitor::SidecarCache {
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

    let channel_control = Arc::new(baybo_gateway::ChannelControlRegistry::new());

    // CLI-driven bot add/remove writes straight to sqlite + vault. The
    // reconciler polls those stores on a short tick and pushes
    // `StartBot` / `StopBot` frames to whichever sidecars are
    // connected. Spawning here so it rides the shared shutdown signal
    // alongside the cron loop and axum servers.
    let bot_reconciler = Arc::new(baybo_gateway::channel::ChannelBotReconciler::new(
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

    // Blind remote-host push: on every completed real user turn, encrypt a
    // per-target preview and POST it to the remote host (C). Always wired — the
    // dispatcher self-gates on its targets: approved device rows (relay path,
    // endpoint recorded at pairing) plus direct-mode web push bindings (the
    // endpoint from the `[push]` config, recorded at `POST /v1/push/register`).
    {
        // Proxy-aware client: /notify + /register POST to the remote host (C), a
        // non-loopback egress target subject to the operator's egress proxy.
        let push_client =
            baybo_security::http::client(boot::proxy_settings(&graph.config).as_ref())?;
        let registrar: Arc<dyn baybo_gateway::push::ApnsRegistrar> = Arc::new(
            baybo_gateway::push::HttpApnsRegistrar::new(push_client.clone()),
        );
        let dispatcher = Arc::new(baybo_gateway::push::PushDispatcher::new(
            graph.stores.device.clone(),
            graph.stores.session.clone(),
            Arc::clone(&graph.session_manager),
            Arc::clone(&graph.secret_vault),
            Arc::new(baybo_gateway::push::HttpNotifySink::new(push_client)),
            Some(registrar),
        ));
        // Second push source: a tool call parked at the approval gate. Nothing
        // reaches the lifecycle bus while it waits — the turn has not
        // completed, and never will unless the user answers — so the gate
        // needs its own path to the dispatcher.
        //
        // Installed on `owner` alone: the queue is per-channel, and the app
        // holds an `owner` connection, so a prompt parked on the TUI's or
        // Telegram's queue is one the phone could not answer even after
        // opening the conversation the push routed it to.
        let (approval_relay, approval_stream) =
            baybo_gateway::push::approval::ApprovalPushRelay::new();
        let approval_wired = match graph
            .channels_registry
            .get(&baybo_model::ChannelType::owner())
        {
            Some(owner) => {
                approval_relay.install(&owner);
                true
            }
            None => false,
        };
        let push_shutdown = shutdown.clone();
        task_tracker.track(baybo_gateway::push::spawn(
            dispatcher,
            Arc::clone(&graph.turn_lifecycle),
            approval_stream,
            async move { push_shutdown.wait().await },
        ));
        tracing::info!(
            approval_pushes = approval_wired,
            "push: dispatcher started (turn lifecycle bus + approval gate)"
        );
    }

    // Deck services: boot starts every enabled card (with the
    // post-upgrade re-gate) and the shutdown edge stops the resident
    // processes.
    {
        let deck = Arc::clone(&graph.deck_manager);
        let deck_shutdown = shutdown.clone();
        task_tracker.track(tokio::spawn(async move {
            deck.boot().await;
            deck_shutdown.wait().await;
            deck.shutdown().await;
        }));
    }

    // Build the axum server from the assembled graph.
    let deps = GatewayDeps {
        config: Arc::clone(&graph.config),
        config_path: reload_config_path,
        runtime_config: runtime_cfg.clone(),
        session_manager: Arc::clone(&graph.session_manager),
        turn_lifecycle: Arc::clone(&graph.turn_lifecycle),
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
        secret_vault: Arc::clone(&graph.secret_vault),
        stores: graph.stores.clone(),
        channel_control,
        bot_reconciler: Arc::clone(&bot_reconciler),
        deck_manager: Arc::clone(&graph.deck_manager),
        workspace_paths: Arc::new(baybo_workspace::WorkspacePaths::new(
            graph.workspace.root.clone(),
        )),
    };

    // Channel loopback-TCP listener — publishes its ephemeral port to
    // `<workspace>/state/channel.port` (next to the singleton
    // lockfile) so TUI and sidecars can discover it without a config
    // roundtrip.
    let channel_server = ChannelServer::bind(&deps, port_file, channel_tokens.clone())
        .map_err(|e| anyhow::anyhow!("bind channel TCP listener: {e}"))?;
    let channel_port = channel_server.port();
    let channel_url = format!("ws://127.0.0.1:{channel_port}/v1/channel-ws");

    // Channel-sidecar supervisor (telegram / weixin / …). The channel
    // TCP listener is already bound above so the kernel's listen queue
    // absorbs child connection attempts even before
    // `channel_server.run()` starts accepting inside the select! below.
    // The browser MCP server lives on a separate path: it is spawned
    // by the MCP reconciler set up inside `runtime::build_managers`,
    // not by this supervisor.
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
            .names_in_domain(baybo_gateway::sidecar::domains::CHANNEL)
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

    // Content control connection: an outbound A->C link so a phone can reach this
    // (possibly NAT'd) gateway for chat via the relay. Self-gates on the approved
    // device row (idle until one is paired). Spawned + tracked here alongside the
    // other managers so it rides the shared shutdown drain rather than leaking as
    // a detached task.
    {
        let relay_content_shutdown = shutdown.clone();
        task_tracker.track(baybo_gateway::spawn_relay_content(
            &deps,
            relay_content_shutdown,
        ));
    }

    let server = GatewayServer::new(deps);
    let banner_bind = server.bind();
    // Dashboard URL first, then the admin token on its own line for the
    // operator to paste into the login field. Deliberately NOT a
    // `?token=…` URL — that would leak the token into the gateway's
    // access log on the very first request.
    println!("Web dashboard: http://{banner_bind}");
    println!("Admin token:   {token}");

    tracing::info!(bind = %banner_bind, "gateway start: all components initialized");

    let admin_shutdown = shutdown.clone();
    let channel_shutdown = shutdown.clone();
    let router_shutdown = shutdown.clone();
    tokio::select! {
        res = server.run(admin_shutdown) => {
            if let Err(e) = res {
                tracing::error!(error = %e, "admin gateway server exited with error");
                shutdown.trigger();
                return Err(anyhow::anyhow!("gateway server error: {e}"));
            }
        }
        res = channel_server.run(channel_shutdown) => {
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

    runtime::force_exit_watchdog(runtime_cfg.shutdown_grace);

    task_tracker.shutdown().await;
    tracing::info!("gateway shutdown complete");
    Ok(())
}
