//! The unified remote-host ("C") binary: serve the relay and (optionally) push
//! roles on a single listener, distinguished by their disjoint route paths
//! (`/notify` + `/register` vs `/pair`, `/content`, `/control`). The **relay is
//! always on**; **push turns on automatically when an APNs `.p8` is configured**
//! (`APNS_P8_PATH` is set). Bind + TLS are configured here (`BIND_ADDR`, and
//! optional `TLS_CERT` + `TLS_KEY` to serve wss/https directly).

use std::process::ExitCode;

use axum::Router;
use remote_host_push::serve::{PushConfig, build_router as push_router};
use remote_host_relay::serve::{RelayConfig, build_router as relay_router};

mod serve;

use serve::TlsPaths;

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

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut app = Router::new();
    let mut roles: Vec<&str> = Vec::new();

    // Push turns on only when an APNs .p8 is configured.
    let p8_configured = std::env::var("APNS_P8_PATH")
        .ok()
        .is_some_and(|p| !p.is_empty());
    if p8_configured {
        let (config, p8_path) = PushConfig::from_env()?;
        let p8_pem = std::fs::read(&p8_path)
            .map_err(|e| format!("read .p8 at {}: {e}", p8_path.display()))?;
        app = app.merge(push_router(&config, &p8_pem)?);
        roles.push("push");
    }

    // Relay is always on (it needs RELAY_INSTANCE_KEYS, validated in from_env).
    let relay = RelayConfig::from_env()?;
    app = app.merge(relay_router(&relay));
    roles.push("relay");

    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| DEFAULT_BIND_ADDR.into());
    let tls = TlsPaths::from_env("TLS_CERT", "TLS_KEY")?;
    let scheme = if tls.is_some() { "https/wss" } else { "http/ws" };
    eprintln!(
        "remote-host: listening on {bind_addr} ({scheme}) — roles: {}",
        roles.join(" + "),
    );
    serve::serve(&bind_addr, tls, app).await?;
    Ok(())
}
