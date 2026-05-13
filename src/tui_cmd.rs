//! CLI entrypoint for the interactive chat loop (`aura tui`).
//!
//! `aura tui` is a thin UI on top of a WS+MessagePack [`WsTransport`]
//! pointed at `aura gateway`'s admin listener (which co-hosts the
//! `/v1/channel-ws` upgrade alongside the bearer-auth admin REST API).
//! Same address the browser-side web chat page uses — one public bind
//! per gateway, no port-file discovery needed.
//!
//! TUI auth is a per-start temporary token the gateway publishes to
//! the secret vault at `gateway.tui_token` (rotated on every
//! `aura gateway start`); the TUI opens the same vault, reads the
//! token, and presents it in the `x-aura-channel-token` header on the
//! WS upgrade. The token is bound to the `tui` label, so the channel-
//! auth middleware admits it through the same path as a sidecar.
//!
//! If the connect fails the command prints a concrete block telling
//! the operator how to start a gateway and exits. The dev-only
//! `--dev-auto-gateway` flag short-circuits that error by spawning
//! one as a subprocess — compiled in only under
//! `cfg(debug_assertions)`, so release builds never see it.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, OnceLock};

use aura_agent::service::ShutdownSignal;
use aura_channels::ChannelError;
use aura_config::AuraConfig;
use aura_gateway::TUI_TOKEN_VAULT_KEY;
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

    let admin_addr = admin_addr_from_config(&config)?;

    let session_id = aura_model::SessionId::from(
        opts.session
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
    );

    // Keep the auto-spawned child alive for the lifetime of the TUI.
    // Dropping the guard sends SIGTERM (with a SIGKILL fallback) so
    // we don't leak a background gateway when the TUI exits.
    #[cfg(debug_assertions)]
    let mut _auto_gateway: Option<dev_auto::AutoGatewayGuard> = None;

    // Read the per-start TUI token from the vault. Done once up front
    // so a missing token (gateway not running yet) flows into the same
    // `NotReachable`-style fallback as an unreachable admin port. In
    // `--dev-auto-gateway` mode the spawned child writes the token
    // before the admin listener accepts connections; we re-read after
    // spawn to pick up the freshly-rotated value.
    let mut tui_token = read_tui_token(&config).await;

    let transport =
        match try_connect_with_token(admin_addr, tui_token.as_deref(), &session_id).await {
            Ok(t) => Arc::new(t),
            Err(err) if !matches!(err, ChannelError::NotReachable(_)) => {
                return Err(unreachable_gateway_error(admin_addr, &err.to_string()));
            }
            Err(err) => {
                #[cfg(debug_assertions)]
                if opts.dev_auto_gateway {
                    // Propagate the parent's resolved config path so the
                    // spawned gateway reads the same workspace — otherwise
                    // a `--config` flag on the TUI would point the child
                    // at a different vault, and they'd disagree on bind
                    // address.
                    let config_path = crate::boot::resolve_config_path();
                    _auto_gateway =
                        Some(dev_auto::spawn_and_wait_ready(admin_addr, config_path).await?);
                    // Reread the freshly-rotated token the spawned gateway
                    // just published.
                    tui_token = read_tui_token(&config).await;
                    Arc::new(
                        try_connect_with_token(admin_addr, tui_token.as_deref(), &session_id)
                            .await
                            .map_err(|e| unreachable_gateway_error(admin_addr, &e.to_string()))?,
                    )
                } else {
                    return Err(unreachable_gateway_error(admin_addr, &err.to_string()));
                }
                #[cfg(not(debug_assertions))]
                {
                    return Err(unreachable_gateway_error(admin_addr, &err.to_string()));
                }
            }
        };
    info!(%admin_addr, "connected to gateway");

    let slash_handler = Arc::new(TuiSlashHandler::new());
    let dashboard_provider = Arc::new(TuiDashboardProvider::new());

    let shutdown = ShutdownSignal::new();
    let tui_shutdown = shutdown.clone();
    let tui = TuiAdapter::new(transport)
        .with_session_id(session_id.clone())
        .with_slash_handler(slash_handler)
        .with_dashboard_provider(dashboard_provider)
        .with_on_exit(Arc::new(move || tui_shutdown.trigger()));

    let _ = tui_log_sink.set(tui.log_sink());

    tui.start().await?;

    info!(%session_id, "TUI session started");

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

/// Resolve the admin listener address from the loaded config. When
/// the gateway is bound to a wildcard interface (`0.0.0.0` / `::`), a
/// same-host TUI rewrites it to loopback — the wildcard is a server-
/// side bind directive, not a dialable target.
fn admin_addr_from_config(config: &AuraConfig) -> anyhow::Result<SocketAddr> {
    let host = config.gateway.bind_address.as_str();
    let ip: IpAddr = host
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid gateway.bind_address {host:?}: {e}"))?;
    let dial_ip = match ip {
        IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        other => other,
    };
    Ok(SocketAddr::new(dial_ip, config.gateway.port))
}

/// Best-effort read of the per-start TUI token from the secret vault.
/// Returns `None` if the vault can't be opened (no encryption key,
/// libsql missing) or the key isn't present yet — both surface to the
/// caller as the same "no live gateway" fallback path. A loud error
/// would only mask the more specific port-file-missing message that
/// the connect attempt produces a moment later.
async fn read_tui_token(config: &AuraConfig) -> Option<String> {
    let vault = match crate::runtime::build_secret_vault(config).await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "tui token: open vault failed");
            return None;
        }
    };
    match vault.get_secret(TUI_TOKEN_VAULT_KEY).await {
        Ok(Some(value)) => match std::str::from_utf8(value.as_bytes()) {
            Ok(s) => Some(s.to_owned()),
            Err(e) => {
                tracing::warn!(error = %e, "tui token in vault is not valid utf-8");
                None
            }
        },
        Ok(None) => {
            tracing::debug!("tui token: vault key absent (gateway not started yet)");
            None
        }
        Err(e) => {
            tracing::debug!(error = %e, "tui token: vault read failed");
            None
        }
    }
}

/// Dial the gateway's admin listener with the supplied TUI token.
/// A missing token (vault key absent / gateway not running yet) is
/// surfaced as `NotReachable` so the caller's existing fallback paths
/// (dev auto-gateway, user-facing error) cover it the same way as a
/// `connect refused`.
async fn try_connect_with_token(
    admin_addr: SocketAddr,
    tui_token: Option<&str>,
    session_id: &aura_model::SessionId,
) -> Result<WsTransport, ChannelError> {
    let token = tui_token.ok_or_else(|| {
        ChannelError::NotReachable(format!(
            "no {TUI_TOKEN_VAULT_KEY} in vault (is the gateway running?)",
        ))
    })?;
    WsTransport::connect(admin_addr, token.to_owned(), session_id.clone()).await
}

fn unreachable_gateway_error(admin_addr: SocketAddr, underlying: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "no aura gateway reachable at {admin_addr}\n  - start it with:       aura gateway start\n  (underlying error: {underlying})"
    )
}

#[cfg(debug_assertions)]
mod dev_auto {
    //! Dev-only: spawn an `aura gateway start` subprocess so a fresh
    //! dev workspace doesn't need a second terminal to run the TUI.
    //! Deliberately isolated in its own module so stripping the
    //! feature also strips the `std::process::Command` call site.

    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::time::Duration;

    use tokio::net::TcpStream;
    use tokio::process::{Child, Command};
    use tracing::info;

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

    /// Spawn `aura gateway start` as a subprocess and poll the admin
    /// listener until a TCP connection succeeds (or the timeout
    /// elapses). A successful `TcpStream::connect` is enough evidence
    /// that the listener is accepting — the caller follows up with
    /// the real WS handshake after this returns.
    pub async fn spawn_and_wait_ready(
        admin_addr: SocketAddr,
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
            if TcpStream::connect(admin_addr).await.is_ok() {
                info!(%admin_addr, "auto-gateway ready");
                return Ok(guard);
            }
            if tokio::time::Instant::now() >= deadline {
                // Drop guard kills the child as we bail.
                let _ = guard.child.take().map(|mut c| c.start_kill());
                anyhow::bail!("auto-gateway did not become reachable at {admin_addr} within 15s",);
            }
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(Duration::from_secs(1));
        }
    }
}
