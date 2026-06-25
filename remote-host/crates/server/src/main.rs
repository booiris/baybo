//! The unified remote-host ("C") binary: serve the relay and (optionally) push
//! roles on a single listener, distinguished by their disjoint route paths
//! (`/notify` + `/register` vs `/pair`, `/content`, `/control`). The **relay is
//! always on**; **push turns on automatically when an APNs `.p8` is configured**
//! (`APNS_P8_PATH` is set). The gateway admission allow-list is a SQLite (libsql)
//! table polled for external edits (`admission_db`). Bind + TLS are configured
//! here (`BIND_ADDR`, and optional `TLS_CERT` + `TLS_KEY` to serve wss/https).

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use remote_host_push::serve::{PushConfig, build_router as push_router};
use remote_host_relay::serve::build_router as relay_router;

mod admission_db;
mod serve;

use serve::TlsPaths;

/// Listener address when `BIND_ADDR` is unset. Map it to the host `:443` (e.g.
/// in docker) for a port-less wss/https URL.
const DEFAULT_BIND_ADDR: &str = "0.0.0.0:7777";
/// Admission-table location when `ADMISSION_DB_PATH` is unset (a mounted volume
/// in docker so an external admin can edit it).
const DEFAULT_DB_PATH: &str = "/data/admission.db";
/// How often to re-read the admission table when `ADMISSION_POLL_SECS` is unset.
const DEFAULT_POLL_SECS: u64 = 30;

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
    // The gateway admission allow-list: a SQLite table, hot-reloaded by polling
    // for external edits. Shared by both roles.
    let db_path = std::env::var("ADMISSION_DB_PATH").unwrap_or_else(|_| DEFAULT_DB_PATH.into());
    let poll = Duration::from_secs(
        std::env::var("ADMISSION_POLL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_POLL_SECS),
    );
    // Track live relay connections so an admission reload that drops a key can
    // kick that gateway's connections, not just refuse new ones — and cap how many
    // connections one instance key may hold (override with MAX_CONNS_PER_INSTANCE).
    let registry = remote_host_relay::ConnectionRegistry::new();
    let registry = match std::env::var("MAX_CONNS_PER_INSTANCE")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
    {
        Some(max) => registry.with_max_per_key(max),
        None => registry,
    };
    let conns = Arc::new(registry);
    let admission = {
        let conns = conns.clone();
        admission_db::open(&db_path, poll, move |revoked| {
            conns.kick(&revoked);
        })
        .await?
    };

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
        app = app.merge(push_router(&config, &p8_pem, admission.clone())?);
        roles.push("push");
    }

    // Relay is always on.
    app = app.merge(relay_router(admission.clone(), conns.clone()));
    roles.push("relay");

    // TODO(dashboard): wire in `remote-host-dashboard` here — a blind,
    // metadata-only status router (counts of admitted instances / device tokens /
    // connected gateways / pending relay legs, never content). It needs a
    // `MetadataProvider` impl over the push `DeviceTokenStore` + the relay
    // `ControlRegistry`/broker, then `app = app.merge(remote_host_dashboard::
    // router(provider))`, likely behind a `DASHBOARD_ENABLE` env gate. The crate
    // compiles as a workspace member today but nothing mounts it.

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
