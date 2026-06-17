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
//! (seeding `runtime::build_leak_detector` with the gateway-owned
//! tokens so any log line that echoes them is masked), and drives the
//! router, admin [`GatewayServer`], and the loopback-TCP
//! [`ChannelServer`] side by side under a shared `ShutdownSignal`.
//! Sidecars register themselves with the channel registry from the WS
//! route task when they connect.

use std::path::PathBuf;
use std::sync::Arc;

use aura_cli::cli::{GatewayCmd, GatewayTokenCmd};
use aura_config::AuraConfig;
use aura_gateway::AdminToken;
use aura_gateway::installer::{self, InstallContext, ServiceInstaller};

use aura_runtime::{boot, runtime};

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
    let log_dir =
        aura_workspace::WorkspacePaths::new(PathBuf::from(&config.workspace.path)).logs_dir();
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
/// 2. `<default_workspace_root>/config/aura.json` — if it already exists.
/// 3. Otherwise `None`, with a loud stderr warning: the service will
///    launch against built-in defaults (no LLM key, default workspace),
///    which is almost never what the user actually wants.
///
/// systemd/launchd don't inherit the invoking shell's env, so the path
/// has to be baked into the unit file at install time — there's no
/// second chance to pick it up later.
fn resolve_install_config_path() -> anyhow::Result<Option<PathBuf>> {
    use aura_workspace::paths::{ENV_CONFIG_PATH, default_config_file};

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
         {ENV_CONFIG_PATH} and re-run `aura gateway install` if that's not what you want.",
        default_path.display(),
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
    let running = aura_runtime::start_gateway(aura_runtime::StartGatewayOpts {
        config,
        config_path: None,
        install_signals: true,
        start_channel_listener: true,
        // The CLI owns tracing: a file layer under the workspace `logs/` dir,
        // with the leak detector wired in so any log line echoing a token is
        // masked. The returned guards ride on `RunningGateway` to stay alive.
        setup_tracing: Box::new(|paths, leak_detector| {
            let log_dir = paths.logs_dir();
            let guards = init_tracing(TracingMode::File {
                log_dir: &log_dir,
                leak_detector,
            });
            let log_buffer = guards.log_buffer();
            (log_buffer, Box::new(guards) as aura_runtime::TracingGuard)
        }),
    })
    .await?;

    // Dashboard URL first, then the admin token on its own line for the
    // operator to paste into the login field. Deliberately NOT a `?token=…`
    // URL — that would leak the token into the gateway's access log on the
    // very first request.
    println!("Web dashboard: http://{}", running.admin_addr);
    println!("Admin token:   {}", running.admin_token);

    running.serve().await
}
