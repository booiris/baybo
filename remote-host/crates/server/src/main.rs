//! The unified remote-host ("C") binary: serve the push and/or relay roles on a
//! single listener, selected by `PUSH_ENABLE` / `RELAY_ENABLE` and distinguished
//! by their disjoint route paths (`/notify` + `/register` vs `/pair`, `/content`,
//! `/control`). Enable one role for an isolated deployment (e.g. the
//! `.p8`-holding push role on its own host), or both to share a port.
//!
//! Bind + TLS are configured once here (`BIND_ADDR`, and optional `TLS_CERT` +
//! `TLS_KEY` to serve wss/https directly); each role contributes only its router.

use std::process::ExitCode;

use axum::Router;
use remote_host_push::serve::{PushConfig, build_router as push_router};
use remote_host_relay::serve::{RelayConfig, build_router as relay_router};
use remote_host_serve::TlsPaths;

/// Listener address when `BIND_ADDR` is unset. Map it to the host `:443` (e.g.
/// in docker) for a port-less wss/https URL.
const DEFAULT_BIND_ADDR: &str = "0.0.0.0:7777";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("remote-host: {e}");
            ExitCode::FAILURE
        }
    }
}

/// A truthy env flag (`1` / `true` / `yes` / `on`, case-insensitive).
fn enabled(var: &str) -> bool {
    std::env::var(var)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let push_on = enabled("PUSH_ENABLE");
    let relay_on = enabled("RELAY_ENABLE");
    if !push_on && !relay_on {
        return Err("enable at least one role: set PUSH_ENABLE=1 and/or RELAY_ENABLE=1".into());
    }

    let mut app = Router::new();
    let mut roles: Vec<&str> = Vec::new();

    if push_on {
        let (config, p8_path) = PushConfig::from_env()?;
        let p8_pem = std::fs::read(&p8_path)
            .map_err(|e| format!("read .p8 at {}: {e}", p8_path.display()))?;
        app = app.merge(push_router(&config, &p8_pem)?);
        roles.push("push");
    }
    if relay_on {
        let config = RelayConfig::from_env()?;
        app = app.merge(relay_router(&config));
        roles.push("relay");
    }

    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| DEFAULT_BIND_ADDR.into());
    let tls = TlsPaths::from_env("TLS_CERT", "TLS_KEY")?;
    let scheme = if tls.is_some() { "https/wss" } else { "http/ws" };
    eprintln!(
        "remote-host: listening on {bind_addr} ({scheme}) — roles: {}",
        roles.join(" + "),
    );
    remote_host_serve::serve(&bind_addr, tls, app).await?;
    Ok(())
}
