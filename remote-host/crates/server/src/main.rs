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
use axum::http::HeaderName;
use remote_host_push::serve::{PushConfig, build_router as push_router};
use remote_host_ratelimit::IpLimitConfig;
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
    // connections one remote_api_key may hold. This is the *fallback* cap used when
    // a key's row leaves `max_conns` NULL; per-key values in the table override it.
    let registry = remote_host_relay::ConnectionRegistry::new();
    let registry = match std::env::var("MAX_CONNS_PER_REMOTE_API_KEY_FALLBACK")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
    {
        Some(max) => registry.with_max_per_key(max),
        None => registry,
    };
    let conns = Arc::new(registry);
    // Two-level content-bandwidth throttle: per-remote_api_key ceiling ∧ per-server
    // sub-cap (see RELAY_BYTES_PER_SEC).
    let bandwidth = Arc::new(remote_host_relay::BandwidthRegistry::new());
    let admission = {
        let conns = conns.clone();
        let bandwidth = bandwidth.clone();
        admission_db::open(&db_path, poll, move |revoked| {
            // A revoked key loses its live connections and all its bandwidth buckets.
            conns.kick(&revoked);
            bandwidth.forget(&revoked);
        })
        .await?
    };

    // The per-source-IP request throttle is **always on**, keying on the socket
    // peer (the primary path terminates TLS here, so the peer is the real client).
    // It is mounted per role with its OWN bucket map: the relay's WS upgrades and
    // push's `/register` + `/notify` are throttled independently. The *client-IP
    // resolution* is shared (one proxy posture for the whole listener):
    //   CLIENT_IP_HEADERS=h1,h2 → resolve the client IP from these headers, in
    //     order, before the socket peer (e.g. `cf-connecting-ip` behind Cloudflare).
    //     Trust them ONLY when the origin is reachable solely via that proxy (CF IP
    //     allowlist / Tunnel / Authenticated Origin Pulls) — else forgeable.
    // Each role's rate / burst / bucket-map cap is independently tunable via env:
    //   RELAY_IP_RATE_PER_SEC / RELAY_IP_BURST / RELAY_IP_BUCKET_CAP
    //   PUSH_IP_RATE_PER_SEC  / PUSH_IP_BURST  / PUSH_IP_BUCKET_CAP
    let trusted_headers: Vec<HeaderName> = std::env::var("CLIENT_IP_HEADERS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .filter_map(|h| HeaderName::from_bytes(h.to_ascii_lowercase().as_bytes()).ok())
        .collect();
    let relay_ip_limit = role_ip_limit(
        "RELAY_IP_RATE_PER_SEC",
        "RELAY_IP_BURST",
        "RELAY_IP_BUCKET_CAP",
        &trusted_headers,
    );
    let push_ip_limit = role_ip_limit(
        "PUSH_IP_RATE_PER_SEC",
        "PUSH_IP_BURST",
        "PUSH_IP_BUCKET_CAP",
        &trusted_headers,
    );

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
        app = app.merge(push_router(&config, &p8_pem, push_ip_limit)?);
        roles.push("push");
    }

    // Relay is always on.
    app = app.merge(relay_router(
        admission.clone(),
        conns.clone(),
        bandwidth.clone(),
        relay_ip_limit,
    ));
    roles.push("relay");

    // Dashboard is intentionally not mounted in this slice.

    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| DEFAULT_BIND_ADDR.into());
    let tls = TlsPaths::from_env("TLS_CERT", "TLS_KEY")?;
    let scheme = if tls.is_some() {
        "https/wss"
    } else {
        "http/ws"
    };
    eprintln!(
        "remote-host: listening on {bind_addr} ({scheme}) — roles: {}",
        roles.join(" + "),
    );
    serve::serve(&bind_addr, tls, app).await?;
    Ok(())
}

/// Build one role's (always-on) per-source-IP throttle config from the shared
/// `trusted_headers` plus that role's rate / burst / bucket-map-cap env overrides
/// (each kept at its default when unset / unparseable / non-positive). Relay and
/// push pass different env names so each is independently sized.
fn role_ip_limit(
    rate_var: &str,
    burst_var: &str,
    cap_var: &str,
    trusted_headers: &[HeaderName],
) -> IpLimitConfig {
    let mut config = IpLimitConfig::with_trusted_headers(trusted_headers.to_vec());
    if let Some(rate) = env_f64(rate_var) {
        config.rate_per_sec = rate;
    }
    if let Some(burst) = env_f64(burst_var) {
        config.burst = burst;
    }
    if let Some(cap) = env_usize(cap_var) {
        config.bucket_soft_cap = cap;
    }
    config
}

/// Parse a positive `f64` env override, else `None`.
fn env_f64(key: &str) -> Option<f64> {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|&v| v > 0.0)
}

/// Parse a positive `usize` env override, else `None`.
fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&v| v > 0)
}
