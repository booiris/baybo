//! CLI entrypoint for `aura gateway …` subcommands.
//!
//! Intercepted from `main.rs` before the normal argv dispatch path
//! (same pattern as `Commands::Tui`). The non-serving subcommands
//! (`install`, `disable`, `uninstall`, `status`, `enable`, `token`) run
//! here with a lightweight boot: they need only the config for path
//! resolution, plus — for the auth-token branch — a `SecretVault`
//! opened against the project's libsql store.
//!
//! `start` is a long-running server: it acquires the per-workspace
//! singleton, builds the full manager graph via [`crate::runtime`]
//! (passing [`BootMode::Gateway`] so the auth token is registered as a
//! log redaction rule), and drives the router, admin [`GatewayServer`],
//! and [`ChannelServer`] UDS listener side by side under a shared
//! `ShutdownSignal`. Sidecars register themselves with the channel
//! registry from the WS route task when they connect.

use std::path::PathBuf;
use std::sync::Arc;

use aura_agent::service::{ShutdownSignal, TaskTracker};
use aura_cli::cli::{GatewayCmd, GatewayTokenCmd};
use aura_config::AuraConfig;
use aura_gateway::installer::{self, InstallContext, ServiceInstaller};
use aura_gateway::{AdminToken, ChannelServer, GatewayDeps, GatewayServer, RuntimeGatewayConfig};
use aura_gateway_auth::{ChannelTokenTable, effective_tui_psk};

use crate::boot;
use crate::runtime;
use crate::singleton;
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
    config: &AuraConfig,
    explicit_exec: Option<PathBuf>,
) -> anyhow::Result<InstallContext> {
    let exec_start = installer::resolve_exec_start(explicit_exec.as_deref())
        .map_err(|e| anyhow::anyhow!("cannot resolve executable path: {e}"))?;
    let config_path = resolve_install_config_path()?;
    let log_dir = PathBuf::from(&config.workspace.path).join("logs");
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
/// 1. `AURA_CONFIG_PATH` — canonicalized; errors if it cannot be
///    resolved to an existing file.
/// 2. `./aura.json` — if present in the current directory.
/// 3. Otherwise `None`, with a loud stderr warning: the service will
///    launch against built-in defaults (no LLM key, default workspace),
///    which is almost never what the user actually wants.
///
/// systemd/launchd don't inherit the invoking shell's env, so the path
/// has to be baked into the unit file at install time — there's no
/// second chance to pick it up later.
fn resolve_install_config_path() -> anyhow::Result<Option<PathBuf>> {
    if let Some(raw) = std::env::var_os("AURA_CONFIG_PATH") {
        let p = PathBuf::from(&raw);
        let canon = p.canonicalize().map_err(|e| {
            anyhow::anyhow!(
                "AURA_CONFIG_PATH={} cannot be resolved ({e}). Fix the path or unset the variable \
                 before running install — a broken path baked into the unit file would only \
                 surface later when the service fails to start.",
                p.display()
            )
        })?;
        return Ok(Some(canon));
    }

    let cwd_default = PathBuf::from("aura.json");
    if cwd_default.exists() {
        let canon = cwd_default
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("./aura.json exists but cannot be canonicalized: {e}"))?;
        eprintln!(
            "install: no AURA_CONFIG_PATH set; pinning service to {}",
            canon.display()
        );
        return Ok(Some(canon));
    }

    eprintln!(
        "install: WARNING — no AURA_CONFIG_PATH and no ./aura.json; the installed service will \
         run with built-in defaults (no LLM key, default workspace). Set AURA_CONFIG_PATH and \
         re-run `aura gateway install` if that's not what you want."
    );
    Ok(None)
}

fn install_service(
    config: &AuraConfig,
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

fn disable(_config: &AuraConfig) -> anyhow::Result<()> {
    let installer = make_installer(true)?;
    installer
        .disable()
        .map_err(|e| anyhow::anyhow!("disable failed: {e}"))?;
    println!("disabled");
    Ok(())
}

fn status(_config: &AuraConfig) -> anyhow::Result<()> {
    let installer = make_installer(true)?;
    let status = installer
        .status()
        .map_err(|e| anyhow::anyhow!("status lookup failed: {e}"))?;
    println!("{status:?}");
    Ok(())
}

async fn uninstall(_config: &AuraConfig) -> anyhow::Result<()> {
    let installer = make_installer(true)?;
    installer
        .uninstall()
        .map_err(|e| anyhow::anyhow!("uninstall failed: {e}"))?;
    println!("uninstalled");
    Ok(())
}

// ---- vault-backed subcommands ----

async fn enable(config: &AuraConfig) -> anyhow::Result<()> {
    let vault = runtime::build_secret_vault(config).await?;
    let token_mgr = AdminToken::new(vault);
    let token = token_mgr.mint_if_absent().await?;

    // Best-effort mark the unit enabled — not fatal if the service
    // isn't installed yet; `enable` is allowed before `install`.
    match make_installer(true) {
        Ok(installer) => {
            if let Err(e) = installer.enable() {
                tracing::warn!(error = %e, "unit not enabled; run `aura gateway install` first");
            }
        }
        Err(e) => tracing::warn!(error = %e, "no installer available"),
    }

    println!("gateway enabled; token: {token}");
    Ok(())
}

async fn token_show(config: &AuraConfig) -> anyhow::Result<()> {
    let vault = runtime::build_secret_vault(config).await?;
    let token_mgr = AdminToken::new(vault);
    match token_mgr.get().await? {
        Some(t) => println!("{t}"),
        None => {
            return Err(anyhow::anyhow!(
                "no token minted; run `aura gateway enable` first"
            ));
        }
    }
    Ok(())
}

async fn token_rotate(config: &AuraConfig) -> anyhow::Result<()> {
    let vault = runtime::build_secret_vault(config).await?;
    let token_mgr = AdminToken::new(vault);
    let token = token_mgr.rotate().await?;
    println!("{token}");
    Ok(())
}

// ---- start (long-running) ----

async fn start(config: Arc<AuraConfig>) -> anyhow::Result<()> {
    // Per-workspace singleton. The gateway owns the same libsql store as
    // the TUI, runs job recovery, and drives cron ticks — two instances
    // against the same workspace would race.
    let workspace_root = PathBuf::from(&config.workspace.path);
    let _workspace_lock = singleton::acquire(workspace_root.as_path())?;

    // Read the auth token BEFORE building the manager graph: the gateway
    // mode registers the token as a `LeakAction::Replace` rule on the
    // LeakDetector, which happens inside `build_managers` before the
    // detector is sealed into an Arc. Auto-mint on first run so a fresh
    // workspace can `aura gateway start` without a prior `enable`.
    let token = {
        let vault = runtime::build_secret_vault(&config).await?;
        AdminToken::new(vault).mint_if_absent().await?
    };

    // Build the leak detector (with the auth token registered as a
    // `LeakAction::Replace` rule) BEFORE initialising tracing so log
    // lines that accidentally echo the token are masked on disk. Pass
    // the same `Arc<LeakDetector>` into `build_managers` so the runtime
    // graph's SecurityGateway uses the same rule set.
    let leak_detector = runtime::build_leak_detector(&config.security, Some(&token));
    let log_dir = workspace_root.join("logs");
    let tracing_guards = init_tracing(TracingMode::File {
        log_dir: &log_dir,
        leak_detector: Arc::clone(&leak_detector),
    });
    let log_buffer = tracing_guards.log_buffer();
    tracing::info!(token_len = token.len(), "gateway token loaded from vault");

    // Resolve the runtime gateway config up front so a bad `bind_address`
    // fails fast before we open libsql a second time.
    let runtime_cfg = RuntimeGatewayConfig::from_config(&config.gateway)
        .map_err(|e| anyhow::anyhow!("invalid gateway config: {e}"))?;

    let shutdown = ShutdownSignal::new();
    let mut graph = runtime::build_managers(
        Arc::clone(&config),
        shutdown.clone(),
        Arc::clone(&leak_detector),
    )
    .await?;
    let run_handle = runtime::wire_router(&mut graph).await;

    // WS-backed sidecars register themselves from the route task when a
    // client connects; nothing to pre-register here.

    let mut task_tracker = TaskTracker::new();
    runtime::install_signal_handler(&mut task_tracker, shutdown.clone());

    // Cron tick loop.
    let cron_handle = Arc::clone(&graph.cron_scheduler);
    task_tracker.track(tokio::spawn(async move {
        cron_handle.run().await;
    }));

    // Shared capability table — the channel UDS listener consumes it
    // for auth, and the WS channel server re-reads it in the
    // Register-frame handshake via `GatewayDeps`.
    let channel_tokens = ChannelTokenTable::new();

    let channel_control = Arc::new(aura_gateway::ChannelControlRegistry::new());

    // CLI-driven bot add/remove writes straight to libsql + vault. The
    // reconciler polls those stores on a short tick and pushes
    // `StartBot` / `StopBot` frames to whichever sidecars are
    // connected. Spawning here so it rides the shared shutdown signal
    // alongside the cron loop and axum servers.
    let bot_reconciler = Arc::new(aura_gateway::channel::ChannelBotReconciler::new(
        Arc::clone(&channel_control),
        Arc::clone(&graph.channel_bot_store),
        Arc::clone(&graph.secret_vault),
    ));
    {
        let reconciler = Arc::clone(&bot_reconciler);
        let shutdown_for_reconciler = shutdown.clone();
        task_tracker.track(tokio::spawn(async move {
            reconciler.run(shutdown_for_reconciler).await;
        }));
    }

    // Build the axum server from the assembled graph.
    let deps = GatewayDeps {
        config: Arc::clone(&graph.config),
        config_path: boot::resolve_config_path(),
        runtime_config: runtime_cfg.clone(),
        session_manager: Arc::clone(&graph.session_manager),
        job_manager: Arc::clone(&graph.job_manager),
        cron_scheduler: Arc::clone(&graph.cron_scheduler),
        memory_manager: Arc::clone(&graph.memory_manager),
        trace_store: Arc::clone(&graph.trace_store),
        skill_registry: Arc::clone(&graph.skill_registry),
        tool_registry: Arc::clone(&graph.tool_registry),
        channel_registry: Arc::clone(&graph.channels_registry),
        llm_client: Arc::clone(&graph.llm_client),
        admin_token: token.clone(),
        log_buffer: Arc::clone(&log_buffer),
        incoming_tx: run_handle.incoming_tx.clone(),
        channel_tokens: channel_tokens.clone(),
        secret_vault: Arc::clone(&graph.secret_vault),
        channel_session_store: Arc::clone(&graph.channel_session_store),
        channel_bot_store: Arc::clone(&graph.channel_bot_store),
        channel_control,
        bot_reconciler: Arc::clone(&bot_reconciler),
        channel_pairing_store: Arc::clone(&graph.channel_pairing_store),
        pairing_pending_ttl_seconds: graph.config.channels.pairing.pending_ttl_seconds,
    };

    // Channel UDS listener — lives under the same workspace identity dir
    // as the singleton lockfile. The PSK is derived with a per-install
    // salt so two copies of the same release binary don't share an
    // effective PSK across machines.
    let channel_server = {
        let psk = effective_tui_psk(workspace_root.as_path())
            .map_err(|e| anyhow::anyhow!("derive channel PSK: {e}"))?;
        let socket_path = workspace_root.join("channel.sock");
        ChannelServer::bind(&deps, socket_path.clone(), psk, channel_tokens)
            .map_err(|e| anyhow::anyhow!("bind channel UDS: {e}"))?
    };

    let server = GatewayServer::new(deps);
    let banner_bind = server.bind();
    println!("Aura gateway listening on http://{banner_bind}");
    println!("  Quick URL: http://{banner_bind}/v1/status?token={token}");

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
