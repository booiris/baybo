//! CLI entrypoint for the interactive chat loop (`baybo tui`).
//!
//! `baybo tui` is a thin UI on top of a WS+MessagePack [`WsTransport`]
//! pointed at `baybo gateway`'s admin listener (which co-hosts the
//! `/v1/channel-ws` upgrade alongside the bearer-auth admin REST API).
//! Same address the browser-side web chat page uses — one public bind
//! per gateway, no port-file discovery needed.
//!
//! TUI auth is a per-start temporary token the gateway publishes to
//! the secret vault at `gateway.tui_token` (rotated on every
//! `baybo gateway start`); the TUI opens the same vault, reads the
//! token, and presents it in the `x-baybo-channel-token` header on the
//! WS upgrade. The token is bound to the `tui` label, which is what the
//! admin listener's co-hosted auth middleware
//! (`require_channel_or_admin_token`) admits — the web chat's admin
//! bearer and a paired device's bearer come through the same door.
//!
//! If the connect fails the command prints a concrete block telling
//! the operator how to start a gateway and exits. The dev-only
//! `--dev-auto-gateway` flag short-circuits that error by spawning
//! one as a subprocess — compiled in only under
//! `cfg(debug_assertions)`, so release builds never see it.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use baybo_agent::service::ShutdownSignal;
use baybo_channels::ChannelError;
use baybo_config::BayboConfig;
use baybo_gateway::AdminToken;
use baybo_tui::client::{TuiDashboardProvider, TuiSlashHandler};
use baybo_tui::{TuiAdapter, TuiLogSink};
use tracing::info;

use crate::gateway_client::{
    admin_addr_from_config, dial_failure_error, read_tui_token, try_connect_with_token,
};
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
pub async fn run(config: Arc<BayboConfig>, opts: Options) -> anyhow::Result<()> {
    let process_manager = baybo_process::ProcessManager::transient();
    let tui_log_sink: Arc<OnceLock<TuiLogSink>> = Arc::new(OnceLock::new());
    let log_dir =
        baybo_workspace::WorkspacePaths::new(PathBuf::from(&config.workspace.path)).logs_dir();
    let _tracing_guards = init_tracing(TracingMode::Tui {
        tui_sink: Arc::clone(&tui_log_sink),
        log_dir: &log_dir,
        leak_detector: crate::runtime::build_leak_detector(&config.security, &[]),
    });
    info!("Baybo - Intelligent Assistant Framework starting");

    let admin_addr = admin_addr_from_config(&config)?;

    let session_id = baybo_model::SessionId::from(
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
    // `--dev-auto-gateway` mode the spawned child rotates this token
    // before its admin listener accepts connections, so that arm
    // re-reads it for the retry connect.
    let tui_token = read_tui_token(&config).await;

    let transport =
        match try_connect_with_token(admin_addr, tui_token.as_deref(), &session_id).await {
            Ok(t) => Arc::new(t),
            Err(err) if !matches!(err, ChannelError::NotReachable(_)) => {
                return Err(dial_failure_error(admin_addr, &err));
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
                    _auto_gateway = Some(
                        dev_auto::spawn_and_wait_ready(
                            Arc::clone(&process_manager),
                            admin_addr,
                            config_path,
                        )
                        .await?,
                    );
                    // Reconnect with the freshly-rotated token the spawned
                    // gateway just published.
                    let tui_token = read_tui_token(&config).await;
                    Arc::new(
                        try_connect_with_token(admin_addr, tui_token.as_deref(), &session_id)
                            .await
                            .map_err(|e| dial_failure_error(admin_addr, &e))?,
                    )
                } else {
                    return Err(dial_failure_error(admin_addr, &err));
                }
                #[cfg(not(debug_assertions))]
                {
                    return Err(dial_failure_error(admin_addr, &err));
                }
            }
        };
    info!(%admin_addr, "connected to gateway");

    // Read the admin bearer token now that the connect has settled — in
    // `--dev-auto-gateway` mode the gateway has spawned and rotated it by
    // this point. Only used for the active-LLM footer query below.
    let admin_token = read_admin_token(&config).await;
    let model_label = fetch_current_model_label(admin_addr, admin_token.as_deref()).await;
    let slash_handler = Arc::new(TuiSlashHandler::new());
    let dashboard_provider = Arc::new(TuiDashboardProvider::new());

    let shutdown = ShutdownSignal::new();
    let tui_shutdown = shutdown.clone();
    let tui = TuiAdapter::new(transport)
        .with_slash_handler(slash_handler)
        .with_dashboard_provider(dashboard_provider)
        .with_model_label(model_label)
        .with_on_exit(Arc::new(move || tui_shutdown.trigger()));

    let _ = tui_log_sink.set(tui.log_sink());

    tui.start().await?;

    info!(%session_id, "TUI session started");

    let mut task_tracker = baybo_agent::service::TaskTracker::new();
    install_signal_handler(&mut task_tracker, shutdown.clone());

    shutdown.wait().await;
    info!("shutdown signal received, stopping TUI");

    // A TUI redraw loop won't block tokio, but the WS pump holds a
    // long-lived read on the channel socket — bound teardown so the
    // process always exits even if the read can't be cancelled
    // promptly.
    force_exit_watchdog(
        Arc::clone(&process_manager),
        std::time::Duration::from_secs(5),
    );

    task_tracker.shutdown().await;
    Ok(())
}

async fn read_admin_token(config: &BayboConfig) -> Option<String> {
    let vault = match crate::runtime::build_secret_vault(config).await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "admin token: open vault failed");
            return None;
        }
    };
    match AdminToken::new(vault).get().await {
        Ok(Some(token)) => Some(token),
        Ok(None) => {
            tracing::debug!("admin token: vault key absent");
            None
        }
        Err(e) => {
            tracing::debug!(error = %e, "admin token: vault read failed");
            None
        }
    }
}

const ACTIVE_LLM_PATH: &str = "/v1/llm";
const ACTIVE_LLM_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

async fn fetch_current_model_label(
    admin_addr: SocketAddr,
    admin_token: Option<&str>,
) -> Option<String> {
    let Some(token) = admin_token else {
        tracing::debug!("active model footer: no admin token available");
        return None;
    };
    let url = format!("http://{admin_addr}{ACTIVE_LLM_PATH}");
    let client = match reqwest::Client::builder()
        .timeout(ACTIVE_LLM_REQUEST_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            tracing::debug!(error = %e, "active model footer: client build failed");
            return None;
        }
    };
    let response = match client.get(&url).bearer_auth(token).send().await {
        Ok(response) => response,
        Err(e) => {
            tracing::debug!(error = %e, "active model footer: request failed");
            return None;
        }
    };
    let status = response.status();
    if !status.is_success() {
        tracing::debug!(%status, "active model footer: gateway returned non-success status");
        return None;
    }
    let payload = match response.json::<serde_json::Value>().await {
        Ok(payload) => payload,
        Err(e) => {
            tracing::debug!(error = %e, "active model footer: invalid response body");
            return None;
        }
    };
    current_model_label_from_payload(&payload)
}

fn current_model_label_from_payload(payload: &serde_json::Value) -> Option<String> {
    let provider = payload
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim();
    let model = payload
        .get("model_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim();
    match (provider.is_empty(), model.is_empty()) {
        (true, true) => None,
        (true, false) => Some(model.to_string()),
        (false, true) => Some(provider.to_string()),
        (false, false) => Some(format!("{provider}/{model}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn current_model_label_formats_provider_and_model() {
        let payload = json!({
            "provider": "openai",
            "model_id": "gpt-5-codex",
        });

        assert_eq!(
            current_model_label_from_payload(&payload).as_deref(),
            Some("openai/gpt-5-codex")
        );
    }

    #[test]
    fn current_model_label_handles_missing_provider() {
        let payload = json!({
            "model_id": "local-model",
        });

        assert_eq!(
            current_model_label_from_payload(&payload).as_deref(),
            Some("local-model")
        );
    }
}

#[cfg(debug_assertions)]
mod dev_auto {
    //! Dev-only: spawn an `baybo gateway start` subprocess so a fresh
    //! dev workspace doesn't need a second terminal to run the TUI.
    //! Deliberately isolated in its own module so stripping the
    //! feature also strips the `std::process::Command` call site.

    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::time::Duration;

    use tokio::net::TcpStream;
    use tokio::process::Command;
    use tracing::info;

    /// RAII guard that kills the spawned gateway when dropped.
    pub struct AutoGatewayGuard {
        child: Option<baybo_process::ManagedChild>,
    }

    /// Spawn `baybo gateway start` as a subprocess and poll the admin
    /// listener until a TCP connection succeeds (or the timeout
    /// elapses). A successful `TcpStream::connect` is enough evidence
    /// that the listener is accepting — the caller follows up with
    /// the real WS handshake after this returns.
    pub async fn spawn_and_wait_ready(
        process_manager: std::sync::Arc<baybo_process::ProcessManager>,
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
        cmd.args(["gateway", "start"])
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let child = process_manager
            .spawn(&mut cmd, "dev-auto-gateway")
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
