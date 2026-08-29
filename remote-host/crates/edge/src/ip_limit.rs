//! Per-client-IP request-rate limiting, shared by the relay's WS routes and the
//! push role's `/register` + `/notify` routes.
//!
//! Each role already has finer-grained controls — the relay's per-rendezvous
//! join limiter + per-`remote_api_key` connection cap, push's per-device notify
//! bucket — but none bounds a single host *spraying requests* across many keys /
//! rendezvous / device ids (or failing admission on each), which can still churn
//! accept/upgrade/verify work. This is the coarse outer backstop: a token bucket
//! per source IP, drawn one token per request, applied **ahead of relay admission
//! or push body parsing** so floods are shed cheaply with `429`.
//!
//! **Deployment caveat.** By default the key is the *socket* peer IP. With
//! remote-host terminating TLS itself that is the real client (or its NAT). Behind
//! a proxy (e.g. Cloudflare) the peer is the proxy's edge, so every client would
//! share one bucket. Two postures fix that, configured via [`IpLimitConfig`]:
//! disable the limiter and rate-limit at the proxy, **or** give it the trusted
//! client-IP header(s) the proxy sets (e.g. `cf-connecting-ip`) so it keys on the
//! real client. Trust such a header **only** when the origin is reachable solely
//! via that proxy — it is otherwise forgeable. The limiter is skipped for a
//! request whose client IP can't be resolved (no trusted header *and* no
//! client-address info, e.g. unit tests served without connect-info).
//!
//! One [`TokenBucket`] per key at a fixed rate, with brim-full (idle) buckets
//! evicted over a soft cap so a churn of distinct source IPs can't grow the map
//! without bound. Time is injectable so the math is tested without real sleeps.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use dashmap::DashMap;

use crate::TokenBucket;

/// Sustained request rate per source IP, in attempts **per second**. Generous for
/// a real client (the app's pair/content connect-retry loops and a gateway's
/// per-turn push posts sit well under it) yet tight enough to clamp a flood from
/// one host.
pub const IP_RATE_PER_SEC: f64 = 10.0;

/// Per-IP burst — attempts that may land back-to-back before pacing kicks in (the
/// app's retry loop and a content session opening in quick succession stay inside
/// it).
pub const IP_BURST: f64 = 60.0;

/// Default soft cap on tracked source-IP buckets (override via the caller's env,
/// e.g. `RELAY_IP_BUCKET_CAP`). When exceeded, brim-full (idle) buckets are
/// evicted — dropping a full bucket grants no extra allowance (a re-created one
/// also starts full), so eviction is free of fairness risk. It bounds the
/// limiter's own map against a churn of distinct source IPs.
pub const IP_BUCKET_SOFT_CAP: usize = 16_384;

/// One token-bucket per source IP, all at the same rate; `rate_per_sec` / `burst`
/// set that rate and `soft_cap` bounds the map's size. The map is a sharded
/// [`DashMap`] so a request flood from many source IPs contends per-shard rather
/// than on one global lock.
pub struct IpRateLimiter {
    buckets: DashMap<IpAddr, TokenBucket>,
    rate_per_sec: f64,
    burst: f64,
    soft_cap: usize,
    /// Latches the over-cap warn to one line per breach episode: set on the
    /// first cap breach that eviction couldn't relieve (fires on the new-IP path,
    /// i.e. during an active many-IP flood), re-armed once the map is back under
    /// the cap.
    over_cap_warned: AtomicBool,
    /// Debounces the per-IP shed warn to one line per [`SHED_WARN_INTERVAL`] per
    /// IP (with a suppressed count), so a single over-rate client is visible at
    /// the default filter without a flood being able to spam warns through it.
    shed_warned: parking_lot::Mutex<HashMap<IpAddr, ShedWarnEntry>>,
}

/// How often the per-IP shed emits at warn (per IP); sheds in between log at
/// debug and are counted into the next warn's `suppressed` field.
const SHED_WARN_INTERVAL: Duration = Duration::from_secs(60);
/// Bound on the shed-warn debounce map; cleared wholesale when a new IP would
/// exceed it (an occasional early re-warn beats unbounded growth).
const SHED_WARN_MAX_KEYS: usize = 4096;

struct ShedWarnEntry {
    last_warn: Instant,
    suppressed: u64,
}

impl IpRateLimiter {
    pub fn new(rate_per_sec: f64, burst: f64, soft_cap: usize) -> Self {
        Self {
            buckets: DashMap::new(),
            rate_per_sec,
            burst,
            soft_cap,
            over_cap_warned: AtomicBool::new(false),
            shed_warned: parking_lot::Mutex::new(HashMap::new()),
        }
    }

    /// Whether this shed for `ip` should log at warn: `Some(suppressed)` at most
    /// once per [`SHED_WARN_INTERVAL`] per IP (with the count debounced since the
    /// last warn), `None` in between (callers log at debug).
    fn shed_should_warn(&self, ip: IpAddr, now: Instant) -> Option<u64> {
        let mut warned = self.shed_warned.lock();
        if warned.len() >= SHED_WARN_MAX_KEYS && !warned.contains_key(&ip) {
            warned.clear();
        }
        match warned.get_mut(&ip) {
            Some(entry) if now.duration_since(entry.last_warn) < SHED_WARN_INTERVAL => {
                entry.suppressed += 1;
                None
            }
            Some(entry) => {
                let suppressed = entry.suppressed;
                entry.last_warn = now;
                entry.suppressed = 0;
                Some(suppressed)
            }
            None => {
                warned.insert(
                    ip,
                    ShedWarnEntry {
                        last_warn: now,
                        suppressed: 0,
                    },
                );
                Some(0)
            }
        }
    }

    /// Account one request for `ip`; returns whether it is within the rate. A
    /// `false` means the caller should refuse with `429`.
    pub fn check(&self, ip: IpAddr) -> bool {
        self.check_at(ip, Instant::now())
    }

    fn check_at(&self, ip: IpAddr, now: Instant) -> bool {
        // Fast path: an existing bucket draws under its own shard only.
        if let Some(mut bucket) = self.buckets.get_mut(&ip) {
            return bucket.try_take_at(1.0, now);
        }
        // New IP: if at the soft cap, shed brim-full (idle) buckets to make room,
        // then insert. `len()`/`retain` are O(shards) but run only on this new-IP
        // path, not for an already-tracked IP.
        if self.buckets.len() >= self.soft_cap {
            self.buckets.retain(|_, b| !b.is_full_at(now));
            let tracked_ips = self.buckets.len();
            if tracked_ips >= self.soft_cap {
                if !self.over_cap_warned.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        tracked_ips,
                        soft_cap = self.soft_cap,
                        "per-IP limiter bucket map exceeds its soft cap with no idle buckets to evict — sustained many-IP flood?"
                    );
                }
            } else if self.over_cap_warned.load(Ordering::Relaxed) {
                self.over_cap_warned.store(false, Ordering::Relaxed);
            }
        } else if self.over_cap_warned.load(Ordering::Relaxed) {
            self.over_cap_warned.store(false, Ordering::Relaxed);
        }
        self.buckets
            .entry(ip)
            .or_insert_with(|| TokenBucket::new_at(self.burst, self.rate_per_sec, now))
            .try_take_at(1.0, now)
    }
}

/// How the per-IP limiter resolves the **client** address of a request.
///
/// `enabled` mounts the limiter at all. `trusted_headers` are header names tried
/// **in order** to read the real client IP (e.g. `["cf-connecting-ip"]` behind
/// Cloudflare) — the first that holds a parseable address wins, else the socket
/// peer ([`ConnectInfo`]) is used. An empty list means socket-peer only.
///
/// **Trust these headers ONLY when the origin is reachable solely via the proxy
/// that sets them** (Cloudflare IP allowlist / Tunnel / Authenticated Origin
/// Pulls). A client header is otherwise forgeable: a direct-to-origin attacker
/// could set any `cf-connecting-ip` to evade the limit or frame an arbitrary IP.
#[derive(Clone)]
pub struct IpLimitConfig {
    pub enabled: bool,
    pub trusted_headers: Vec<HeaderName>,
    /// Sustained per-source-IP request rate (defaults to [`IP_RATE_PER_SEC`]).
    pub rate_per_sec: f64,
    /// Per-source-IP burst (defaults to [`IP_BURST`]).
    pub burst: f64,
    /// Soft cap on the per-source-IP bucket map (defaults to [`IP_BUCKET_SOFT_CAP`]).
    pub bucket_soft_cap: usize,
}

impl Default for IpLimitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            trusted_headers: Vec::new(),
            rate_per_sec: IP_RATE_PER_SEC,
            burst: IP_BURST,
            bucket_soft_cap: IP_BUCKET_SOFT_CAP,
        }
    }
}

impl IpLimitConfig {
    /// Limiter off entirely.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    /// Limiter on, keying on the socket peer (no proxy-header trust). Correct when
    /// remote-host terminates TLS itself (the peer is the real client).
    pub fn socket_peer() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    /// Limiter on, resolving the client IP from `headers` (tried in order) before
    /// falling back to the socket peer. See the type docs for the trust caveat. The
    /// `rate_per_sec` / `burst` / `bucket_soft_cap` fields are then set directly by
    /// the caller from its env overrides.
    pub fn with_trusted_headers(headers: Vec<HeaderName>) -> Self {
        Self {
            enabled: true,
            trusted_headers: headers,
            ..Self::default()
        }
    }
}

/// The limiter + trusted-header set wired into the middleware, independent of the
/// router's own state so it composes onto any [`Router`].
#[derive(Clone)]
struct IpLimitState {
    limiter: Arc<IpRateLimiter>,
    trusted_headers: Arc<Vec<HeaderName>>,
}

/// Mount the per-IP throttle on `router` as its **outermost** layer (tower runs
/// outer→inner), so a flood is shed by client IP before admission and any
/// per-route work. A `disabled` config returns the router untouched.
///
/// Add this *after* the router's other `route_layer`s so it wraps them: when
/// remote-host terminates TLS the socket peer is the real client; behind a proxy,
/// either disable it and rate-limit at the proxy, or give it the trusted
/// client-IP header(s) the proxy sets — see the [`IpLimitConfig`] trust caveat.
pub fn apply(router: Router, config: IpLimitConfig) -> Router {
    if !config.enabled {
        return router;
    }
    let state = IpLimitState {
        limiter: Arc::new(IpRateLimiter::new(
            config.rate_per_sec,
            config.burst,
            config.bucket_soft_cap,
        )),
        trusted_headers: Arc::new(config.trusted_headers),
    };
    router.route_layer(middleware::from_fn_with_state(state, limit_per_ip))
}

/// `429 Too Many Requests` for a source IP over its request rate.
fn too_many() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        "too many requests from this address",
    )
        .into_response()
}

/// Resolve the request's **client** IP: try each trusted proxy header in order
/// (the first parseable address wins), else the socket peer from [`ConnectInfo`].
/// `None` only when neither yields an address (no trusted header present *and* the
/// server carries no client-address info, e.g. a unit test served without
/// connect-info) — the caller then skips the limiter.
///
/// For a single-value header (`cf-connecting-ip`) the whole value is the IP; for a
/// list-valued one (`x-forwarded-for`) the **left-most** token is the original
/// client. Both are only trustworthy when the origin is locked to the proxy (see
/// [`IpLimitConfig`]).
pub fn resolve_client_ip(req: &Request, trusted_headers: &[HeaderName]) -> Option<IpAddr> {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip());
    resolve_client_ip_from(req.headers(), peer, trusted_headers)
}

/// [`resolve_client_ip`] from the request's parts — for a handler that holds the
/// [`HeaderMap`] and the socket `peer` (via an `Option<ConnectInfo<SocketAddr>>`
/// extractor) rather than the whole `Request` (e.g. the relay content handlers,
/// which resolve the leg's client IP to attribute its bytes). Same trust caveat.
pub fn resolve_client_ip_from(
    headers: &HeaderMap,
    peer: Option<IpAddr>,
    trusted_headers: &[HeaderName],
) -> Option<IpAddr> {
    for name in trusted_headers {
        let Some(value) = headers.get(name) else {
            continue;
        };
        let text = value.to_str().ok();
        if let Some(first) = text.map(|s| s.split(',').next().unwrap_or("").trim())
            && let Ok(ip) = first.parse::<IpAddr>()
        {
            return Some(ip);
        }
        // The value is attacker-controllable — truncate before echoing it.
        // `to_str` guarantees visible ASCII, so byte slicing can't split a char.
        let shown = text.unwrap_or("<not ascii>");
        tracing::debug!(
            header = %name,
            value_prefix = &shown[..shown.len().min(TRUSTED_HEADER_VALUE_LOG_MAX)],
            "trusted client-IP header present but unparseable; falling back"
        );
    }
    peer
}

/// Cap on how much of an unparseable trusted-header value the fallback debug log
/// echoes — the value is attacker-controllable.
const TRUSTED_HEADER_VALUE_LOG_MAX: usize = 64;

/// Outermost pre-layer: throttle requests per client IP (flood backstop, ahead of
/// admission). The client IP comes from the configured trusted proxy header(s) if
/// present, else the socket peer; when neither is available the limiter is skipped
/// so behaviour is unchanged there.
async fn limit_per_ip(State(state): State<IpLimitState>, req: Request, next: Next) -> Response {
    if let Some(ip) = resolve_client_ip(&req, &state.trusted_headers)
        && !state.limiter.check(ip)
    {
        // One warn per IP per interval (a single over-rate client stays visible
        // at the default filter); the sheds in between log at debug so a flood
        // can't spam the default-level log through this line.
        let endpoint = crate::ip_traffic::endpoint_label(req.uri().path());
        match state.limiter.shed_should_warn(ip, Instant::now()) {
            Some(suppressed) => tracing::warn!(
                %ip,
                endpoint,
                rate_per_sec = state.limiter.rate_per_sec,
                burst = state.limiter.burst,
                suppressed,
                "per-IP rate limit exceeded; refusing with 429"
            ),
            None => tracing::debug!(
                %ip,
                endpoint,
                rate_per_sec = state.limiter.rate_per_sec,
                burst = state.limiter.burst,
                "per-IP rate limit exceeded; refusing with 429"
            ),
        }
        return too_many();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use std::time::Duration;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([127, 0, 0, n])
    }

    fn limiter() -> IpRateLimiter {
        IpRateLimiter::new(IP_RATE_PER_SEC, IP_BURST, IP_BUCKET_SOFT_CAP)
    }

    #[test]
    fn allows_the_burst_then_throttles_then_recovers() {
        let limiter = limiter();
        let t0 = Instant::now();
        // The whole burst lands back-to-back in the same instant.
        for _ in 0..IP_BURST as usize {
            assert!(limiter.check_at(ip(1), t0));
        }
        // The next one over the burst is refused.
        assert!(!limiter.check_at(ip(1), t0), "burst exhausted -> 429");
        // Tokens refill at the sustained rate.
        assert!(limiter.check_at(ip(1), t0 + Duration::from_secs(1)));
    }

    #[test]
    fn buckets_are_per_ip() {
        let limiter = limiter();
        let t0 = Instant::now();
        for _ in 0..IP_BURST as usize {
            assert!(limiter.check_at(ip(1), t0));
        }
        assert!(!limiter.check_at(ip(1), t0), "ip(1) drained");
        // A different source IP has its own independent budget.
        assert!(limiter.check_at(ip(2), t0), "distinct IP unaffected");
    }

    #[test]
    fn soft_cap_evicts_idle_ip_buckets() {
        // A tiny configured cap: once the map is full of brim-full (idle) buckets,
        // a new source IP's retain sweeps them, so a churn of IPs can't grow it.
        let limiter = IpRateLimiter::new(IP_RATE_PER_SEC, IP_BURST, 2);
        let t0 = Instant::now();
        assert!(limiter.check_at(ip(1), t0));
        assert!(limiter.check_at(ip(2), t0));
        // A second later ip(1)/ip(2) have refilled to the brim; the next IP's
        // retain evicts them, bounding the map at the cap.
        let later = t0 + Duration::from_secs(1);
        assert!(limiter.check_at(ip(3), later));
        assert!(
            limiter.buckets.len() <= 2,
            "idle IP buckets evicted at the cap"
        );
    }

    #[test]
    fn resolve_client_ip_prefers_trusted_header_then_falls_back() {
        let cf = HeaderName::from_static("cf-connecting-ip");
        let xff = HeaderName::from_static("x-forwarded-for");
        let peer = SocketAddr::from(([10, 0, 0, 9], 4000));
        let trusted = [cf.clone(), xff.clone()];

        let build = |headers: &[(&str, &str)]| {
            let mut b = Request::builder();
            for (k, v) in headers {
                b = b.header(*k, *v);
            }
            let mut req = b.body(Body::empty()).unwrap();
            req.extensions_mut().insert(ConnectInfo(peer));
            req
        };

        // The trusted CF header wins over the socket peer.
        assert_eq!(
            resolve_client_ip(&build(&[("cf-connecting-ip", "203.0.113.7")]), &trusted),
            Some("203.0.113.7".parse().unwrap())
        );
        // CF absent, XFF present → the left-most token is the original client.
        assert_eq!(
            resolve_client_ip(
                &build(&[("x-forwarded-for", "203.0.113.8, 70.0.0.1")]),
                &trusted
            ),
            Some("203.0.113.8".parse().unwrap())
        );
        // A malformed header value falls through to the socket peer.
        assert_eq!(
            resolve_client_ip(&build(&[("cf-connecting-ip", "not-an-ip")]), &trusted),
            Some(peer.ip())
        );
        // No trusted headers configured → socket peer.
        assert_eq!(resolve_client_ip(&build(&[]), &[]), Some(peer.ip()));
    }
}
