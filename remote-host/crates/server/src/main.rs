//! The unified remote-host ("C") binary: serve the relay and (optionally) push
//! roles on a single listener, distinguished by their disjoint route paths
//! (`/notify` + `/register` vs `/pair`, `/content`, `/control`). The **relay is
//! always on**; **push turns on automatically when an APNs `.p8` is configured**
//! (`APNS_P8_PATH` is set). The gateway admission allow-list is a SQLite
//! table polled for external edits (`admission_db`). Bind + TLS are configured
//! here (`BIND_ADDR`, and optional `TLS_CERT` + `TLS_KEY` to serve wss/https); the
//! operator dashboard's separate listener takes its own optional `DASHBOARD_TLS_CERT`
//! + `DASHBOARD_TLS_KEY`, defaulting to plain HTTP.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::HeaderName;
use remote_host_edge::{IpLimitConfig, IpTrafficRegistry, ip_traffic};
use remote_host_protocol::key_tag;
use remote_host_push::serve::{PushConfig, build_router as push_router};
use remote_host_push::{DeviceTokenStore, InMemoryDeviceTokenStore};
use remote_host_relay::serve::{RelayServices, build_router as relay_router};

mod admission_db;
mod dashboard_backend;
mod logging;
mod serve;
mod sqlite;
mod traffic;
mod traffic_query;

use serve::TlsPaths;

/// Listener address when `BIND_ADDR` is unset. Map it to the host `:443` (e.g.
/// in docker) for a port-less wss/https URL.
const DEFAULT_BIND_ADDR: &str = "0.0.0.0:7777";
/// Listener address for the operator dashboard when `DASHBOARD_BIND_ADDR` is
/// unset. A SEPARATE listener from the relay/push one (never behind Cloudflare):
/// plain HTTP by default — front it with a trusted network or an SSH tunnel — or
/// its own in-process TLS via `DASHBOARD_TLS_CERT` + `DASHBOARD_TLS_KEY`.
const DEFAULT_DASHBOARD_BIND_ADDR: &str = "0.0.0.0:7778";
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
    // Process start instant; the dashboard overview derives uptime + a wall-clock
    // start time from it.
    let started_at = std::time::Instant::now();

    // The gateway relay admission allow-list: a SQLite table, hot-reloaded by
    // polling for external edits. Push requests carry the same header for an
    // upstream edge to inspect, but the application handlers do not resolve it.
    let db_path = std::env::var("ADMISSION_DB_PATH").unwrap_or_else(|_| DEFAULT_DB_PATH.into());
    let poll = Duration::from_secs(
        env_override("ADMISSION_POLL_SECS", |_: &u64| true).unwrap_or(DEFAULT_POLL_SECS),
    );
    // Track live relay connections so an admission reload that drops a key can
    // kick that gateway's connections, not just refuse new ones — and cap how many
    // connections one remote_api_key may hold. This is the *fallback* cap used when
    // a key's row leaves `max_conns` NULL; per-key values in the table override it.
    // Fallback per-key connection cap (used when a key's row leaves `max_conns`
    // NULL); shared by the connection registry and the traffic-cap sizing below.
    let conns_fallback: usize =
        env_override("MAX_CONNS_PER_REMOTE_API_KEY_FALLBACK", |_: &usize| true)
            .unwrap_or(remote_host_relay::conns::DEFAULT_MAX_CONNS_PER_KEY);
    let conns =
        Arc::new(remote_host_relay::ConnectionRegistry::new().with_max_per_key(conns_fallback));
    // Two-level content-bandwidth throttle: per-remote_api_key ceiling ∧ per-server
    // sub-cap (see RELAY_BYTES_PER_SEC).
    let bandwidth = Arc::new(remote_host_relay::BandwidthRegistry::new());
    // Connected gateway control channels and the matching/piping broker; both are
    // threaded into the relay router (instead of being created inside it) so the
    // dashboard overview can read their connected/pending counts.
    let control = Arc::new(remote_host_relay::ControlRegistry::new());
    let broker = Arc::new(remote_host_relay::RelayBroker::new());
    // Per-(remote_api_key, server_id) traffic counters; the relay records into them
    // on the data path and the flush task (below) drains them to the durable ledger.
    let relay_traffic = Arc::new(remote_host_relay::TrafficRegistry::new());
    // The revoke hook (shared by the poller and the dashboard's `force_reload`): a
    // revoked key loses its live connections and all its bandwidth buckets. It lives
    // here, where `conns`/`bandwidth` live, so `admission_db` keeps no relay dep.
    let revoke_hook: admission_db::RevokeHook = {
        let conns = conns.clone();
        let bandwidth = bandwidth.clone();
        Arc::new(move |revoked: std::collections::HashSet<String>| {
            let kicked_conns = conns.kick(&revoked);
            bandwidth.forget(&revoked);
            let revoked_key_tags: Vec<String> = revoked.iter().map(|k| key_tag(k)).collect();
            tracing::debug!(
                revoked = revoked.len(),
                kicked_conns,
                revoked_key_tags = ?revoked_key_tags,
                "remote-host: revoked keys — kicked their live relay connections and dropped their bandwidth buckets"
            );
        })
    };
    let admission_db = Arc::new(admission_db::open(&db_path, poll, revoke_hook).await?);
    let admission = admission_db.admission();

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
        .filter_map(|h| {
            HeaderName::from_bytes(h.to_ascii_lowercase().as_bytes())
                .inspect_err(|_| {
                    tracing::warn!(
                        entry = %h,
                        env_var = "CLIENT_IP_HEADERS",
                        "remote-host: CLIENT_IP_HEADERS entry is not a valid header name; ignoring it (client-IP resolution will not consult it)"
                    );
                })
                .ok()
        })
        .collect();
    // The effective client-IP posture for the boot log: the trusted header list,
    // or the socket peer when none are configured.
    let trusted_ip_headers = if trusted_headers.is_empty() {
        "socket-peer".to_string()
    } else {
        trusted_headers
            .iter()
            .map(HeaderName::as_str)
            .collect::<Vec<_>>()
            .join(",")
    };
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
    // Captured for the boot log — the configs themselves move into the routers.
    let (relay_ip_rate_per_sec, relay_ip_burst) =
        (relay_ip_limit.rate_per_sec, relay_ip_limit.burst);
    let (push_ip_rate_per_sec, push_ip_burst) = (push_ip_limit.rate_per_sec, push_ip_limit.burst);

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
    // The live device-binding registry: built here (not inside `build_router`) so the
    // same handle backs both the push routes and the dashboard's read-only devices
    // page. Stays `None` while push is off.
    let mut device_store: Option<Arc<dyn DeviceTokenStore>> = None;
    let p8_configured = std::env::var("APNS_P8_PATH")
        .ok()
        .is_some_and(|p| !p.is_empty());
    if p8_configured {
        let (config, p8_path) = PushConfig::from_env()?;
        let p8_pem = std::fs::read(&p8_path)
            .map_err(|e| format!("read .p8 at {}: {e}", p8_path.display()))?;
        let pt = Arc::new(remote_host_push::PushTrafficRegistry::new());
        let store: Arc<dyn DeviceTokenStore> = Arc::new(InMemoryDeviceTokenStore::with_limits(
            config.limits.device_store_cap,
            config.limits.unconfirmed_ttl,
        ));
        app = app.merge(push_router(
            &config,
            &p8_pem,
            store.clone(),
            pt.clone(),
            push_ip_limit,
        )?);
        push_traffic = Some(pt);
        device_store = Some(store);
        roles.push("push");
    } else {
        tracing::info!(
            "remote-host: push role disabled — APNS_P8_PATH not configured; /notify and /register will return 404"
        );
    }

    // Relay is always on.
    app = app.merge(relay_router(
        RelayServices {
            admission: admission.clone(),
            conns: conns.clone(),
            bandwidth: bandwidth.clone(),
            traffic: relay_traffic.clone(),
            ip_traffic: ip_traffic.clone(),
            control: control.clone(),
            broker: broker.clone(),
        },
        relay_ip_limit,
    ));
    roles.push("relay");

    // The durable traffic ledger path, read here so the dashboard's read-only
    // reader and the flush task below share one value (the flush task consumes the
    // owned string). An empty value disables persistence.
    let traffic_db_path =
        std::env::var("TRAFFIC_DB_PATH").unwrap_or_else(|_| DEFAULT_TRAFFIC_DB_PATH.into());

    // The fallback per-key connection cap as a `u32`: both the admission traffic-cap
    // sizing closure and the dashboard backend consume it (`u32` is `Copy`, so one
    // conversion serves both sites).
    let conns_fallback = u32::try_from(conns_fallback).unwrap_or(u32::MAX);

    // The operator dashboard runs on its OWN listener (`DASHBOARD_BIND_ADDR`) —
    // NOT merged into the relay/push app, never behind Cloudflare, and given no
    // per-IP recorder/limiter (nothing fronts it, so the socket peer is the
    // client). Plain HTTP by default, or its own in-process TLS via the
    // `DASHBOARD_TLS_*` pair. It stays off until `DASHBOARD_TOKEN` is set; any
    // non-empty value enables it.
    let dashboard: Option<Router> = if let Some(token) = dashboard_token() {
        let traffic_reader = traffic_query::TrafficReader::open(&traffic_db_path).await?;
        let backend = Arc::new(dashboard_backend::RuntimeDashboardBackend::from_config(
            dashboard_backend::DashboardBackendConfig {
                admission_db: admission_db.clone(),
                traffic_reader,
                conns: conns.clone(),
                control: control.clone(),
                broker: broker.clone(),
                relay_traffic: relay_traffic.clone(),
                ip_traffic: ip_traffic.clone(),
                devices: device_store.clone(),
                push_enabled: p8_configured,
                started_at,
                build_version: env!("CARGO_PKG_VERSION"),
            },
        ));
        roles.push("dashboard");
        Some(remote_host_dashboard::router(
            remote_host_dashboard::DashboardConfig { backend, token },
        ))
    } else {
        None
    };

    // Mount the per-IP request recorder as the OUTERMOST layer, so it counts every
    // request (and its body bytes) by client IP + endpoint before either role's
    // per-IP rate limiter can shed it. Same trusted-header client-IP resolution as
    // the limiters.
    app = ip_traffic::apply(app, ip_traffic.clone(), trusted_headers.clone());

    // Drain the relay (and push, if on) traffic counters to the durable ledger every
    // TRAFFIC_FLUSH_SECS; the same task's eviction bounds the in-memory maps. The
    // ledger path was read above (shared with the dashboard reader).
    let traffic_flush_secs =
        env_override("TRAFFIC_FLUSH_SECS", |_: &u64| true).unwrap_or(DEFAULT_TRAFFIC_FLUSH_SECS);
    let traffic_retention_days = env_override("TRAFFIC_RETENTION_DAYS", |_: &u64| true)
        .unwrap_or(DEFAULT_TRAFFIC_RETENTION_DAYS);
    // Size the relay traffic entry cap to 2× the live admission connection capacity
    // (each (key, server) entry needs a content leg; entries linger ~5 min past a
    // leg's close → ×2). The flush task re-evaluates this so the cap follows
    // hot-reloaded admission edits instead of a fixed magic number.
    let admission_for_cap = admission.clone();
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
    // The dashboard runs on its own listener (never fronted by Cloudflare). It
    // defaults to plain HTTP — the token travels cleartext, so it belongs on a
    // trusted network or behind an SSH tunnel — but can terminate its own TLS
    // in-process when DASHBOARD_TLS_CERT + DASHBOARD_TLS_KEY are both set.
    let dash_addr =
        std::env::var("DASHBOARD_BIND_ADDR").unwrap_or_else(|_| DEFAULT_DASHBOARD_BIND_ADDR.into());
    match dashboard {
        Some(dash) => {
            let dashboard_tls = TlsPaths::from_env("DASHBOARD_TLS_CERT", "DASHBOARD_TLS_KEY")?;
            let dash_scheme = if dashboard_tls.is_some() {
                "https"
            } else {
                "http"
            };
            tracing::info!(
                %bind_addr,
                dashboard = %dash_addr,
                %scheme,
                dashboard_scheme = %dash_scheme,
                roles = %roles.join(" + "),
                %trusted_ip_headers,
                relay_ip_rate_per_sec,
                relay_ip_burst,
                push_ip_rate_per_sec,
                push_ip_burst,
                admission_db_path = %db_path,
                admission_poll_secs = poll.as_secs(),
                keys_admitted = admission.len(),
                "remote-host: listening",
            );
            tokio::try_join!(
                serve::serve(&bind_addr, tls.clone(), app),
                serve::serve(&dash_addr, dashboard_tls, dash),
            )?;
        }
        None => {
            tracing::info!(
                %bind_addr,
                %scheme,
                roles = %roles.join(" + "),
                %trusted_ip_headers,
                relay_ip_rate_per_sec,
                relay_ip_burst,
                push_ip_rate_per_sec,
                push_ip_burst,
                admission_db_path = %db_path,
                admission_poll_secs = poll.as_secs(),
                keys_admitted = admission.len(),
                "remote-host: listening",
            );
            serve::serve(&bind_addr, tls, app).await?;
        }
    }
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

/// Resolve the optional operator-dashboard bearer token from `DASHBOARD_TOKEN`.
/// Unset or empty → `None` (the dashboard stays off). Any non-empty value enables
/// the dashboard — there is no length requirement.
fn dashboard_token() -> Option<remote_host_dashboard::DashboardToken> {
    std::env::var("DASHBOARD_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
        .map(remote_host_dashboard::DashboardToken::new)
}

/// Parse an env override, accepting only values `is_valid` passes. `None` (the
/// caller's default applies) when the var is unset or blank — Compose passes
/// `${VAR:-}` as the empty string — and, with a **warn**, when it is set to
/// something unparseable or rejected, so a typo'd knob is self-diagnosing
/// instead of silently falling back.
fn env_override<T: std::str::FromStr>(key: &str, is_valid: impl Fn(&T) -> bool) -> Option<T> {
    let raw = std::env::var(key).ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    match raw.parse::<T>() {
        Ok(v) if is_valid(&v) => Some(v),
        _ => {
            tracing::warn!(
                env_var = key,
                raw_value = %raw,
                "remote-host: env override is set but invalid (unparseable or out of range); using the default"
            );
            None
        }
    }
}

/// Parse a positive `f64` env override, else `None` (warning when set but invalid).
fn env_f64(key: &str) -> Option<f64> {
    env_override(key, |v: &f64| *v > 0.0)
}

/// Parse a positive `usize` env override, else `None` (warning when set but invalid).
fn env_usize(key: &str) -> Option<usize> {
    env_override(key, |v: &usize| *v > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN_VAR: &str = "DASHBOARD_TOKEN";

    /// Restore (or clear) `DASHBOARD_TOKEN` on drop so this test never leaks a value
    /// into the shared process env that a later test might read.
    struct EnvGuard(Option<String>);
    impl EnvGuard {
        fn capture() -> Self {
            Self(std::env::var(TOKEN_VAR).ok())
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.0 {
                // SAFETY: single-threaded test teardown; no other test reads this var.
                Some(v) => unsafe { std::env::set_var(TOKEN_VAR, v) },
                None => unsafe { std::env::remove_var(TOKEN_VAR) },
            }
        }
    }

    /// `dashboard_token`: unset/empty → `None` (dashboard off); any non-empty value
    /// (even a short one) → `Some` — there is no length requirement.
    #[test]
    fn dashboard_token_gate() {
        let _guard = EnvGuard::capture();

        // SAFETY: serial mutation inside one test; the guard restores on drop.
        unsafe { std::env::remove_var(TOKEN_VAR) };
        assert!(dashboard_token().is_none(), "unset → off");

        unsafe { std::env::set_var(TOKEN_VAR, "") };
        assert!(dashboard_token().is_none(), "blank → off");

        unsafe { std::env::set_var(TOKEN_VAR, "x") };
        assert!(dashboard_token().is_some(), "short non-empty → on");

        unsafe { std::env::set_var(TOKEN_VAR, "0123456789abcdef0123456789abcdef") };
        assert!(dashboard_token().is_some(), "long non-empty → on");
    }
}
