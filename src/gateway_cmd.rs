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
//! log redaction rule), registers an `HttpAdapter` into the channel
//! registry, and drives the router and [`GatewayServer`] side by side
//! under a shared `ShutdownSignal`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aura_agent::service::{ShutdownSignal, TaskTracker};
use aura_cli::cli::{GatewayCmd, GatewayTokenCmd};
use aura_config::AuraConfig;
use aura_gateway::installer::{self, InstallContext, ServiceInstaller};
use aura_gateway::{GatewayDeps, GatewayServer, GatewayToken, HttpAdapter, RuntimeGatewayConfig};
use aura_security::{LeakDetector, RedactingMakeWriter};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::boot;
use crate::runtime;
use crate::singleton;

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
    let token_mgr = GatewayToken::new(vault);
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
    let token_mgr = GatewayToken::new(vault);
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
    let token_mgr = GatewayToken::new(vault);
    let token = token_mgr.rotate().await?;
    println!("{token}");
    Ok(())
}

// ---- tracing init for `start` ----

/// Install a tracing subscriber for the gateway process.
///
/// Writes to `<log_dir>/aura-gateway.log` through a
/// [`RedactingMakeWriter`] so tokens/secrets matching any detector rule
/// are masked before landing on disk. `AURA_LOG_FORMAT=json` produces
/// structured output; any other value uses the default text format.
/// Tracing is `init()`-ed here because the `start` subcommand is
/// intercepted in `main.rs` before the TUI's `init_tracing` runs — so
/// without this the server would emit zero logs.
///
/// The returned [`WorkerGuard`] must be held for the lifetime of the
/// process: dropping it flushes and stops the non-blocking appender.
fn init_gateway_tracing(log_dir: &Path, leak_detector: Arc<LeakDetector>) -> Option<WorkerGuard> {
    if let Err(e) = std::fs::create_dir_all(log_dir) {
        eprintln!(
            "warning: could not create gateway log dir {}: {e}. Logs will go to stderr only.",
            log_dir.display()
        );
        let env_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("aura=info"));
        if let Err(e) = tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt::layer().with_ansi(false))
            .try_init()
        {
            eprintln!("warning: tracing subscriber already initialized: {e}");
        }
        return None;
    }
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("aura=info"));
    let json = std::env::var("AURA_LOG_FORMAT").unwrap_or_default() == "json";
    let appender = tracing_appender::rolling::daily(log_dir, "aura-gateway.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let writer = RedactingMakeWriter::new(leak_detector, writer);
    let fmt_layer = fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
        .with_target(true)
        .with_file(true)
        .with_line_number(true);
    let result = if json {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt_layer.json().with_span_list(true))
            .try_init()
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt_layer)
            .try_init()
    };
    if let Err(e) = result {
        eprintln!("warning: tracing subscriber already initialized: {e}");
    }
    Some(guard)
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
        GatewayToken::new(vault).mint_if_absent().await?
    };

    // Build the leak detector (with the auth token registered as a
    // `LeakAction::Replace` rule) BEFORE initialising tracing so log
    // lines that accidentally echo the token are masked on disk. Pass
    // the same `Arc<LeakDetector>` into `build_managers` so the runtime
    // graph's SecurityGateway uses the same rule set.
    let leak_detector = runtime::build_leak_detector(&config.security, Some(&token));
    let _tracing_guard =
        init_gateway_tracing(&workspace_root.join("logs"), Arc::clone(&leak_detector));
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

    // Register the HTTP adapter and start channel background tasks.
    let http_adapter = Arc::new(HttpAdapter::new());
    {
        let mut reg = graph.channels_registry.write().await;
        reg.register(Arc::clone(&http_adapter) as Arc<dyn aura_channels::ChannelAdapter>)?;
        reg.start_all(run_handle.incoming_tx.clone()).await?;
    }

    let mut task_tracker = TaskTracker::new();
    runtime::install_signal_handler(&mut task_tracker, shutdown.clone());

    // Cron tick loop.
    let cron_handle = Arc::clone(&graph.cron_scheduler);
    task_tracker.track(tokio::spawn(async move {
        cron_handle.run().await;
    }));

    // Build the axum server from the assembled graph.
    let deps = GatewayDeps {
        config: Arc::clone(&graph.config),
        config_path: boot::resolve_config_path(),
        runtime_config: runtime_cfg.clone(),
        adapter: Arc::clone(&http_adapter),
        session_manager: Arc::clone(&graph.session_manager),
        job_manager: Arc::clone(&graph.job_manager),
        cron_scheduler: Arc::clone(&graph.cron_scheduler),
        memory_manager: Arc::clone(&graph.memory_manager),
        trace_store: Arc::clone(&graph.trace_store),
        skill_registry: Arc::clone(&graph.skill_registry),
        tool_registry: Arc::clone(&graph.tool_registry),
        channel_registry: Arc::clone(&graph.channels_registry),
        llm_client: Arc::clone(&graph.llm_client),
        auth_token: token.clone(),
    };
    let server = GatewayServer::new(deps);

    let banner_bind = server.bind();
    println!("Aura gateway listening on http://{banner_bind}");
    println!("  Quick URL: http://{banner_bind}/v1/status?token={token}");

    tracing::info!(bind = %banner_bind, "gateway start: all components initialized");

    let server_shutdown = shutdown.clone();
    let router_shutdown = shutdown.clone();
    tokio::select! {
        res = server.run(server_shutdown) => {
            if let Err(e) = res {
                tracing::error!(error = %e, "gateway server exited with error");
                return Err(anyhow::anyhow!("gateway server error: {e}"));
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

