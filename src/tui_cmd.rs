//! CLI entrypoint for the interactive chat loop (`aura tui`).
//!
//! `aura tui` is a thin UI on top of a WS+MessagePack [`WsTransport`]
//! pointed at `aura gateway`'s channel listener — the gateway holds
//! the workspace singleton, the manager graph, and the router.
//!
//! Port discovery: the gateway writes `<workspace>/channel.port` on
//! bind; both sides read the same file so they agree on the loopback
//! port without any config roundtrip. TUI auth is a per-install PSK
//! derived via `aura_gateway_auth::effective_tui_psk`; both ends read
//! the same on-disk salt to arrive at the same key.
//!
//! If the connect fails the command prints a concrete block telling
//! the operator how to start a gateway and exits. The dev-only
//! `--dev-auto-gateway` flag short-circuits that error by spawning
//! one as a subprocess — compiled in only under
//! `cfg(debug_assertions)`, so release builds never see it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use aura_agent::service::ShutdownSignal;
use aura_channels::ChannelError;
use aura_config::AuraConfig;
use aura_gateway_auth::effective_tui_psk;
use aura_tui::client::{TuiDashboardProvider, TuiSlashHandler, WsTransport};
use aura_tui::{TuiAdapter, TuiLogSink};
use tracing::info;

use crate::runtime::force_exit_watchdog;
use crate::runtime::install_signal_handler;
use crate::tracing_init::{TracingMode, init_tracing};

/// Resolved options passed from `main.rs` after parsing the clap
/// struct-variant.
pub struct Options {
    pub session: Option<String>,
    #[cfg(debug_assertions)]
    pub dev_auto_gateway: bool,
}

/// Run the interactive TUI to completion. Returns once the event
/// loop exits (user typed `/quit`, adapter closed) or the shared
/// shutdown signal fires (SIGINT/SIGTERM).
pub async fn run(config: Arc<AuraConfig>, opts: Options) -> anyhow::Result<()> {
    let tui_log_sink: Arc<OnceLock<TuiLogSink>> = Arc::new(OnceLock::new());
    let _tracing_guards = init_tracing(TracingMode::Tui {
        tui_sink: Arc::clone(&tui_log_sink),
    });
    info!("Aura - Intelligent Assistant Framework starting");

    let (port_file, psk) = resolve_channel_auth(&config)?;

    let session_id = opts
        .session
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // Keep the auto-spawned child alive for the lifetime of the TUI.
    // Dropping the guard sends SIGTERM (with a SIGKILL fallback) so
    // we don't leak a background gateway when the TUI exits.
    #[cfg(debug_assertions)]
    let mut _auto_gateway: Option<dev_auto::AutoGatewayGuard> = None;

    let transport = match try_connect(&port_file, &psk, &session_id).await {
        Ok(t) => Arc::new(t),
        Err(err) if !matches!(err, ChannelError::NotReachable(_)) => {
            return Err(unreachable_gateway_error(&port_file, &err.to_string()));
        }
        Err(err) => {
            #[cfg(debug_assertions)]
            if opts.dev_auto_gateway {
                // Propagate the parent's resolved config path so the
                // spawned gateway reads the same workspace — otherwise
                // a `--config` flag on the TUI would point the child
                // at a different vault, and they'd disagree on the
                // port file.
                let config_path = crate::boot::resolve_config_path();
                _auto_gateway =
                    Some(dev_auto::spawn_and_wait_ready(&port_file, config_path).await?);
                Arc::new(
                    try_connect(&port_file, &psk, &session_id)
                        .await
                        .map_err(|e| unreachable_gateway_error(&port_file, &e.to_string()))?,
                )
            } else {
                return Err(unreachable_gateway_error(&port_file, &err.to_string()));
            }
            #[cfg(not(debug_assertions))]
            {
                return Err(unreachable_gateway_error(&port_file, &err.to_string()));
            }
        }
    };
    info!(
        port_file = %port_file.display(),
        "connected to gateway"
    );

    let slash_handler = Arc::new(TuiSlashHandler::new());
    let dashboard_provider = Arc::new(TuiDashboardProvider::new());

    let shutdown = ShutdownSignal::new();
    let tui_shutdown = shutdown.clone();
    let tui = TuiAdapter::new()
        .with_transport(transport)
        .with_session_id(session_id.clone())
        .with_slash_handler(slash_handler)
        .with_dashboard_provider(dashboard_provider)
        .with_on_exit(Arc::new(move || tui_shutdown.trigger()));

    let _ = tui_log_sink.set(tui.log_sink());

    tui.start().await?;

    info!(session_id, "TUI session started");

    let mut task_tracker = aura_agent::service::TaskTracker::new();
    install_signal_handler(&mut task_tracker, shutdown.clone());

    shutdown.wait().await;
    info!("shutdown signal received, stopping TUI");

    // A TUI redraw loop won't block tokio, but the WS pump holds a
    // long-lived read on the channel socket — bound teardown so the
    // process always exits even if the read can't be cancelled
    // promptly.
    force_exit_watchdog(std::time::Duration::from_secs(5));

    task_tracker.shutdown().await;
    Ok(())
}

/// Derive the channel port-file path and effective TUI PSK from the
/// workspace config. The file is fixed at `<workspace>/channel.port`
/// — not configurable, so gateway and TUI resolve it identically.
/// The PSK is mixed with the per-install salt file the gateway
/// writes on its first start; if the salt is missing we fall back
/// to creating it here so `aura tui --dev-auto-gateway` works on a
/// fresh workspace.
fn resolve_channel_auth(config: &AuraConfig) -> anyhow::Result<(PathBuf, [u8; 32])> {
    let workspace_root = PathBuf::from(&config.workspace.path);
    let port_file = workspace_root.join("channel.port");
    let psk = effective_tui_psk(workspace_root.as_path())
        .map_err(|e| anyhow::anyhow!("derive channel PSK: {e}"))?;
    Ok((port_file, psk))
}

/// Read the gateway's published port. Absence of the file is treated
/// as `NotReachable` so the caller's existing fallback paths (dev
/// auto-gateway, user-facing error) work without a new code path.
async fn try_connect(
    port_file: &Path,
    psk: &[u8; 32],
    session_id: &str,
) -> Result<WsTransport, ChannelError> {
    let port = read_port(port_file).ok_or_else(|| {
        ChannelError::NotReachable(format!(
            "no channel.port at {} (is the gateway running?)",
            port_file.display(),
        ))
    })?;
    WsTransport::connect(port, *psk, session_id.to_owned()).await
}

fn read_port(port_file: &Path) -> Option<u16> {
    std::fs::read_to_string(port_file).ok()?.trim().parse().ok()
}

fn unreachable_gateway_error(port_file: &Path, underlying: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "no aura gateway reachable (port file: {port})\n  - start it with:       aura gateway start\n  (underlying error: {underlying})",
        port = port_file.display()
    )
}

#[cfg(debug_assertions)]
mod dev_auto {
    //! Dev-only: spawn an `aura gateway start` subprocess so a fresh
    //! dev workspace doesn't need a second terminal to run the TUI.
    //! Deliberately isolated in its own module so stripping the
    //! feature also strips the `std::process::Command` call site.

    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::time::Duration;

    use tokio::net::TcpStream;
    use tokio::process::{Child, Command};
    use tracing::info;

    use super::read_port;

    /// RAII guard that kills the spawned gateway when dropped.
    pub struct AutoGatewayGuard {
        child: Option<Child>,
    }

    impl Drop for AutoGatewayGuard {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                // `start_kill` is non-blocking SIGKILL on Unix, which
                // is what we want from a Drop impl: no async context
                // and no opportunity to hang. Gateway's own graceful
                // shutdown runs on its SIGTERM handler; for dev use
                // the abrupt stop is acceptable.
                let _ = child.start_kill();
            }
        }
    }

    /// Spawn `aura gateway start` as a subprocess and poll the
    /// channel.port file + the loopback TCP port until a connection
    /// succeeds (or the timeout elapses). A successful
    /// `TcpStream::connect` is enough evidence that the listener is
    /// accepting — the caller follows up with the real WS handshake
    /// after this returns.
    pub async fn spawn_and_wait_ready(
        port_file: &Path,
        config_path: Option<PathBuf>,
    ) -> anyhow::Result<AutoGatewayGuard> {
        // Loud banner so nobody mistakes the dev convenience for a
        // real deployment. Prints before the TUI's alternate-screen
        // takes over so the operator sees it scrolling past in their
        // shell.
        eprintln!("DEV: auto-started gateway for TUI — not for production");

        let exe = std::env::current_exe()
            .map_err(|e| anyhow::anyhow!("read current_exe for auto-gateway: {e}"))?;
        let mut cmd = Command::new(&exe);
        if let Some(path) = config_path.as_deref() {
            cmd.arg("--config").arg(path);
        }
        let child = cmd
            .args(["gateway", "start"])
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn {}: {e}", exe.display()))?;

        let mut guard = AutoGatewayGuard { child: Some(child) };

        // Exponential-ish poll: 100ms, 200ms, 400ms, 800ms, capped
        // at 1000ms. 15s total budget matches the design doc.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut wait = Duration::from_millis(100);
        loop {
            if let Some(port) = read_port(port_file) {
                let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
                if TcpStream::connect(addr).await.is_ok() {
                    info!(port, port_file = %port_file.display(), "auto-gateway ready");
                    return Ok(guard);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                // Drop guard kills the child as we bail.
                let _ = guard.child.take().map(|mut c| c.start_kill());
                anyhow::bail!(
                    "auto-gateway did not become reachable (port file: {}) within 15s",
                    port_file.display()
                );
            }
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(Duration::from_secs(1));
        }
    }
}
