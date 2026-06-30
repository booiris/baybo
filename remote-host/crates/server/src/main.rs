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
use remote_host_edge::{IpLimitConfig, IpTrafficRegistry, ip_traffic};
use remote_host_push::serve::{PushConfig, build_router as push_router};
use remote_host_relay::serve::{RelayServices, build_router as relay_router};

mod admission_db;
mod logging;
mod serve;
mod traffic;

use serve::TlsPaths;

/// Listener address when `BIND_ADDR` is unset. Map it to the host `:443` (e.g.
/// in docker) for a port-less wss/https URL.
const DEFAULT_BIND_ADDR: &str = "0.0.0.0:7777";
/// Admission-table location when `ADMISSION_DB_PATH` is unset (a mounted volume
/// in docker so an external admin can edit it).
const DEFAULT_DB_PATH: &str = "/data/admission.db";
/// How often to re-read the admission table when `ADMISSION_POLL_SECS` is unset.
const DEFAULT_POLL_SECS: u64 = 30;
/// Where the durable traffic ledger lives when `TRAFFIC_DB_PATH` is unset (on the
/// `/data` volume so it outlives container recreation). An *empty* value disables
/// persistence — the in-memory counters still drain + evict, just to nowhere.
const DEFAULT_TRAFFIC_DB_PATH: &str = "/data/traffic.db";
/// Traffic flush + eviction cadence (seconds) when `TRAFFIC_FLUSH_SECS` is unset.
const DEFAULT_TRAFFIC_FLUSH_SECS: u64 = 60;
/// Days of hourly traffic history retained when `TRAFFIC_RETENTION_DAYS` is unset.
const DEFAULT_TRAFFIC_RETENTION_DAYS: u64 = 60;

#[tokio::main]
async fn main() -> ExitCode {
    // Install the subscriber before anything logs; hold the guard so the
    // non-blocking file writer flushes on exit (including the fatal arm below).
    let _log_guard = logging::init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("remote-host: {e}");
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
    // Fallback per-key connection cap (used when a key's row leaves `max_conns`
    // NULL); shared by the connection registry and the traffic-cap sizing below.
    let conns_fallback: usize = std::env::var("MAX_CONNS_PER_REMOTE_API_KEY_FALLBACK")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(remote_host_relay::conns::DEFAULT_MAX_CONNS_PER_KEY);
    let conns =
        Arc::new(remote_host_relay::ConnectionRegistry::new().with_max_per_key(conns_fallback));
    // Two-level content-bandwidth throttle: per-remote_api_key ceiling ∧ per-server
    // sub-cap (see RELAY_BYTES_PER_SEC).
    let bandwidth = Arc::new(remote_host_relay::BandwidthRegistry::new());
    // Per-(remote_api_key, server_id) traffic counters; the relay records into them
    // on the data path and the flush task (below) drains them to the durable ledger.
    let relay_traffic = Arc::new(remote_host_relay::TrafficRegistry::new());
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

    // Per-(ip, endpoint) traffic counters: a recorder middleware (mounted as the
    // outermost layer below) counts every request + its body bytes, and the relay's
    // content legs add their relayed bytes to the same entries.
    let ip_traffic = Arc::new(IpTrafficRegistry::new());

    let mut app = Router::new();
    let mut roles: Vec<&str> = Vec::new();

    // Push turns on only when an APNs .p8 is configured; its per-device traffic
    // registry is created here so it can be handed to both the router and the flush
    // task (it stays `None` while push is off).
    let mut push_traffic: Option<Arc<remote_host_push::PushTrafficRegistry>> = None;
    let p8_configured = std::env::var("APNS_P8_PATH")
        .ok()
        .is_some_and(|p| !p.is_empty());
    if p8_configured {
        let (config, p8_path) = PushConfig::from_env()?;
        let p8_pem = std::fs::read(&p8_path)
            .map_err(|e| format!("read .p8 at {}: {e}", p8_path.display()))?;
        let pt = Arc::new(remote_host_push::PushTrafficRegistry::new());
        app = app.merge(push_router(&config, &p8_pem, pt.clone(), push_ip_limit)?);
        push_traffic = Some(pt);
        roles.push("push");
    }

    // Relay is always on.
    app = app.merge(relay_router(
        RelayServices {
            admission: admission.clone(),
            conns: conns.clone(),
            bandwidth: bandwidth.clone(),
            traffic: relay_traffic.clone(),
            ip_traffic: ip_traffic.clone(),
        },
        relay_ip_limit,
    ));
    roles.push("relay");

    // Dashboard is intentionally not mounted in this slice.

    // Mount the per-IP request recorder as the OUTERMOST layer, so it counts every
    // request (and its body bytes) by client IP + endpoint before either role's
    // per-IP rate limiter can shed it. Same trusted-header client-IP resolution as
    // the limiters.
    app = ip_traffic::apply(app, ip_traffic.clone(), trusted_headers.clone());

    // Drain the relay (and push, if on) traffic counters to the durable ledger every
    // TRAFFIC_FLUSH_SECS; the same task's eviction bounds the in-memory maps.
    let traffic_db_path =
        std::env::var("TRAFFIC_DB_PATH").unwrap_or_else(|_| DEFAULT_TRAFFIC_DB_PATH.into());
    let traffic_flush_secs = std::env::var("TRAFFIC_FLUSH_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_TRAFFIC_FLUSH_SECS);
    let traffic_retention_days = std::env::var("TRAFFIC_RETENTION_DAYS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_TRAFFIC_RETENTION_DAYS);
    // Size the relay traffic entry cap to 2× the live admission connection capacity
    // (each (key, server) entry needs a content leg; entries linger ~5 min past a
    // leg's close → ×2). The flush task re-evaluates this so the cap follows
    // hot-reloaded admission edits instead of a fixed magic number.
    let admission_for_cap = admission.clone();
    let conns_fallback = u32::try_from(conns_fallback).unwrap_or(u32::MAX);
    let relay_traffic_max_tracked = move || {
        admission_for_cap
            .total_max_conns(conns_fallback)
            .saturating_mul(2) as usize
    };
    traffic::spawn(
        traffic_db_path,
        traffic_flush_secs,
        traffic_retention_days,
        relay_traffic,
        push_traffic,
        ip_traffic,
        relay_traffic_max_tracked,
    );

    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| DEFAULT_BIND_ADDR.into());
    let tls = TlsPaths::from_env("TLS_CERT", "TLS_KEY")?;
    let scheme = if tls.is_some() {
        "https/wss"
    } else {
        "http/ws"
    };
    tracing::info!(
        %bind_addr,
        %scheme,
        roles = %roles.join(" + "),
        "remote-host: listening",
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
