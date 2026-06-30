//! Per-source-IP traffic accounting: how many times each client IP called each
//! endpoint, and how many bytes it moved.
//!
//! Two recording paths feed one `(ip, endpoint)` entry:
//! 1. a request-recording middleware (mounted on every route) bumps the **request
//!    count** and adds the request body's `Content-Length` — uniform across all
//!    endpoints, per request. The map is a sharded [`DashMap`], so concurrent
//!    requests from different IPs lock different shards rather than one global lock —
//!    contention under a flood scales with cores;
//! 2. the relay's content legs additionally attribute their **relayed bytes** to
//!    the IP via a cheap [`IpByteMeter`] handle (an `Arc` to the same counters,
//!    resolved once per leg), so the forward hot path is a lock-free `fetch_add`.
//!
//! A single off-hot-path flush task drains the registry on an interval — the same
//! snapshot-then-commit + idle-eviction discipline as the relay's traffic registry
//! — so an in-flight leg's bytes land in the *current* hour bucket every interval
//! rather than dumping into the hour the leg happens to close in.
//!
//! The `endpoint` is a **stable route label** (e.g. `content/join`, `notify`), not
//! the param-filled path, so cardinality is the fixed route set and the middleware
//! agrees with the relay legs on the key.

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering, fence};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderName, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use dashmap::DashMap;

use crate::ip_limit::resolve_client_ip;

/// A request's resolved client IP, or [`Unknown`](Self::Unknown) when none could be
/// resolved (no trusted proxy header and no connect-info). Either way the traffic is
/// still recorded — `Unknown` lands in the `"unknown"` bucket rather than being
/// dropped. `Copy`, so it keys the map with no allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClientIp {
    Addr(IpAddr),
    Unknown,
}

impl ClientIp {
    /// `Some(ip) → Addr(ip)`, `None → Unknown` — for callers holding an optional
    /// resolved address (the recorder middleware and the relay content handlers).
    pub fn from_opt(ip: Option<IpAddr>) -> Self {
        ip.map_or(Self::Unknown, Self::Addr)
    }
}

impl std::fmt::Display for ClientIp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Addr(a) => a.fmt(f),
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

/// Stable per-endpoint labels — shared between the request middleware (which maps a
/// concrete path to one of these) and the relay content legs (which name theirs
/// directly), so both accumulate onto the same `(ip, endpoint)` entry.
pub const EP_PAIR_HOST: &str = "pair/host";
pub const EP_PAIR_JOIN: &str = "pair/join";
pub const EP_CONTROL: &str = "control";
pub const EP_CONTENT_JOIN: &str = "content/join";
pub const EP_CONTENT_HOST: &str = "content/host";
pub const EP_NOTIFY: &str = "notify";
pub const EP_REGISTER: &str = "register";
pub const EP_STATUS: &str = "status";
/// Anything not on the known route set — bounds endpoint cardinality so a spray of
/// junk paths can't blow up the label space.
pub const EP_OTHER: &str = "other";

/// Default ceiling on simultaneously-tracked `(ip, endpoint)` entries. Source IPs
/// are unbounded (any client), so this hard cap + idle eviction bound the map
/// against an IP-churn flood, mirroring the per-IP rate-limiter's bucket cap. Sized
/// for many client IPs × the handful of route labels.
pub const DEFAULT_IP_MAX_TRACKED: usize = 65_536;

/// Default consecutive zero-traffic flush intervals before a fully-drained entry
/// with no live leg is evicted (~5 min at the 60s flush cadence).
pub const DEFAULT_IP_IDLE_EVICT_EPOCHS: u32 = 5;

/// Map a concrete request path to its stable [endpoint label](self). Unknown paths
/// collapse to [`EP_OTHER`] so the label space is the fixed route set.
pub fn endpoint_label(path: &str) -> &'static str {
    let p = path.strip_suffix('/').unwrap_or(path);
    if p == "/control" {
        EP_CONTROL
    } else if p == "/notify" {
        EP_NOTIFY
    } else if p == "/register" {
        EP_REGISTER
    } else if p == "/status" {
        EP_STATUS
    } else if p.starts_with("/pair/host/") {
        EP_PAIR_HOST
    } else if p.starts_with("/pair/join/") {
        EP_PAIR_JOIN
    } else if p.starts_with("/content/join/") {
        EP_CONTENT_JOIN
    } else if p.starts_with("/content/host/") {
        EP_CONTENT_HOST
    } else {
        EP_OTHER
    }
}

/// One `(ip, endpoint)`'s live counters, shared via `Arc` with any relay leg
/// metering bytes against it. Incremented `Relaxed` (a single flusher reads them).
#[derive(Default, Debug)]
struct IpCounters {
    requests: AtomicU64,
    bytes: AtomicU64,
}

impl IpCounters {
    fn load(&self) -> IpCounts {
        IpCounts {
            requests: self.requests.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
        }
    }
}

/// A plain set of per-`(ip, endpoint)` counts: a per-interval delta from
/// [`collect`](IpTrafficRegistry::collect) or a cumulative total from
/// [`snapshot`](IpTrafficRegistry::snapshot).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IpCounts {
    pub requests: u64,
    pub bytes: u64,
}

impl IpCounts {
    fn delta_since(&self, base: &IpCounts) -> IpCounts {
        IpCounts {
            requests: self.requests.saturating_sub(base.requests),
            bytes: self.bytes.saturating_sub(base.bytes),
        }
    }

    fn is_zero(&self) -> bool {
        *self == IpCounts::default()
    }
}

/// One delta row from [`collect`](IpTrafficRegistry::collect).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IpTrafficDelta {
    pub ip: String,
    pub endpoint: String,
    pub counts: IpCounts,
}

struct Entry {
    counters: Arc<IpCounters>,
    flushed: IpCounts,
    pending: IpCounts,
    idle_epochs: u32,
}

/// Per-`(ip, endpoint)` request/byte counters, bounded by [`DEFAULT_IP_MAX_TRACKED`]
/// and reclaimed by the flush task's idle eviction. A sharded [`DashMap`] so the
/// per-request recorder path scales with cores under a flood; keyed by
/// `(ClientIp, &'static str)` — both `Copy` — so it allocates nothing.
pub struct IpTrafficRegistry {
    map: DashMap<(ClientIp, &'static str), Entry>,
    max_tracked: usize,
    idle_evict_epochs: u32,
}

impl Default for IpTrafficRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl IpTrafficRegistry {
    pub fn new() -> Self {
        Self {
            map: DashMap::new(),
            max_tracked: DEFAULT_IP_MAX_TRACKED,
            idle_evict_epochs: DEFAULT_IP_IDLE_EVICT_EPOCHS,
        }
    }

    /// Construct with explicit bounds (tests exercise the cap + eviction cheaply).
    pub fn with_limits(max_tracked: usize, idle_evict_epochs: u32) -> Self {
        Self {
            map: DashMap::new(),
            max_tracked: max_tracked.max(1),
            idle_evict_epochs,
        }
    }

    /// The current entry ceiling — so the flush task can warn when the map saturates
    /// (new `(ip, endpoint)` pairs then go unrecorded).
    pub fn max_tracked(&self) -> usize {
        self.max_tracked
    }

    /// Get or (under the cap) create the `(ip, endpoint)` entry's shared counters.
    /// `None` once the map is full and the pair is new — the caller then records
    /// nothing rather than growing the map.
    ///
    /// An already-tracked pair resolves with a single sharded `get` (concurrent with
    /// requests to other shards/IPs). Only a brand-new pair pays the `len()` cap check
    /// (O(shards), but never on the repeat-IP path) + a one-shard `entry` insert. The
    /// `(ClientIp, &'static str)` key is `Copy`, so nothing is allocated here.
    fn counters_for(&self, ip: ClientIp, endpoint: &'static str) -> Option<Arc<IpCounters>> {
        let key = (ip, endpoint);
        if let Some(entry) = self.map.get(&key) {
            return Some(Arc::clone(&entry.counters));
        }
        if self.map.len() >= self.max_tracked {
            return None; // at the soft cap, a new pair records nothing
        }
        // `or_insert_with` is a no-op if another thread inserted between the `get`
        // and here. The cap may be overshot by the few in-flight new-pair inserters
        // (a soft bound), which is fine.
        let entry = self.map.entry(key).or_insert_with(|| Entry {
            counters: Arc::new(IpCounters::default()),
            flushed: IpCounts::default(),
            pending: IpCounts::default(),
            idle_epochs: 0,
        });
        Some(Arc::clone(&entry.counters))
    }

    /// Record one request to `endpoint` from `ip` carrying `bytes` of request body
    /// (the middleware path — per request; a single sharded `get` for an
    /// already-tracked pair). Counts the call and adds the request bytes. `endpoint`
    /// is a stable route label.
    pub fn record_request(&self, ip: ClientIp, endpoint: &'static str, bytes: u64) {
        if let Some(counters) = self.counters_for(ip, endpoint) {
            counters.requests.fetch_add(1, Ordering::Relaxed);
            counters.bytes.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    /// A byte meter for a relay leg from `ip` on `endpoint`: the leg adds its
    /// relayed bytes via the returned handle (lock-free per frame). Resolved once
    /// per leg, never on the hot path; inert once the map is at its cap.
    pub fn meter_for(&self, ip: ClientIp, endpoint: &'static str) -> IpByteMeter {
        IpByteMeter {
            counters: self.counters_for(ip, endpoint),
        }
    }

    /// Snapshot each entry once and return the non-zero deltas since the last
    /// [`commit`](Self::commit), stashing the snapshot as `pending`. Mutates only the
    /// flush task's private `pending`; does not insert/remove during iteration.
    pub fn collect(&self) -> Vec<IpTrafficDelta> {
        let mut out = Vec::new();
        for mut entry in self.map.iter_mut() {
            let live = entry.counters.load();
            let delta = live.delta_since(&entry.flushed);
            entry.pending = live;
            if !delta.is_zero() {
                let (ip, endpoint) = *entry.key();
                out.push(IpTrafficDelta {
                    ip: ip.to_string(),
                    endpoint: endpoint.to_string(),
                    counts: delta,
                });
            }
        }
        out
    }

    /// Advance the baseline to the last [`collect`](Self::collect)'s snapshot (call
    /// only after its deltas were durably written), roll the idle bookkeeping, and
    /// evict a fully-drained entry with no live leg. `retain` holds each shard's write
    /// lock while running, so the `strong_count` check is race-free against a
    /// concurrent `record_request` on that shard.
    pub fn commit(&self) {
        let evict_epochs = self.idle_evict_epochs;
        self.map.retain(|_, entry| {
            let had_delta = entry.pending != entry.flushed;
            entry.flushed = entry.pending;
            entry.idle_epochs = if had_delta {
                0
            } else {
                entry.idle_epochs.saturating_add(1)
            };
            if entry.idle_epochs < evict_epochs || Arc::strong_count(&entry.counters) != 1 {
                return true;
            }
            // Pair the Relaxed strong-count read with the departed leg's Arc-drop
            // Release so the final byte write is observed before eviction.
            fence(Ordering::Acquire);
            entry.counters.load() != entry.flushed
        });
    }

    /// Cumulative-since-start counts per tracked entry (tests / future dashboard).
    pub fn snapshot(&self) -> Vec<IpTrafficDelta> {
        self.map
            .iter()
            .map(|entry| {
                let (ip, endpoint) = *entry.key();
                IpTrafficDelta {
                    ip: ip.to_string(),
                    endpoint: endpoint.to_string(),
                    counts: entry.counters.load(),
                }
            })
            .collect()
    }

    /// Number of entries currently tracked (test/observability helper).
    pub fn tracked_len(&self) -> usize {
        self.map.len()
    }
}

/// A cheap handle on one relay leg's `(ip, endpoint)` byte counter (the request
/// count is recorded separately by the middleware). Inert (`None`) past the cap.
#[derive(Clone)]
pub struct IpByteMeter {
    counters: Option<Arc<IpCounters>>,
}

impl IpByteMeter {
    /// Account `nbytes` of relayed traffic. Lock-free: one `fetch_add(Relaxed)`.
    #[inline]
    pub fn record(&self, nbytes: usize) {
        if let Some(counters) = &self.counters {
            counters.bytes.fetch_add(nbytes as u64, Ordering::Relaxed);
        }
    }
}

/// The request-recording middleware's state: the registry + the same trusted
/// client-IP headers the limiter uses (so both resolve the same client IP).
#[derive(Clone)]
struct RecordState {
    registry: Arc<IpTrafficRegistry>,
    trusted_headers: Arc<Vec<HeaderName>>,
}

/// Mount the per-IP request recorder on `router` as a layer. It bumps the
/// `(ip, endpoint)` request count + request `Content-Length` for **every** request;
/// one whose client IP can't be resolved (no trusted header and no connect-info,
/// e.g. a test) is recorded under the `"unknown"` IP rather than dropped. Apply it
/// as an outer layer so it sees all requests, including ones a downstream limiter
/// sheds.
pub fn apply(
    router: Router,
    registry: Arc<IpTrafficRegistry>,
    trusted_headers: Vec<HeaderName>,
) -> Router {
    let state = RecordState {
        registry,
        trusted_headers: Arc::new(trusted_headers),
    };
    router.layer(middleware::from_fn_with_state(state, record_ip_traffic))
}

async fn record_ip_traffic(State(state): State<RecordState>, req: Request, next: Next) -> Response {
    let ip = ClientIp::from_opt(resolve_client_ip(&req, &state.trusted_headers));
    let endpoint = endpoint_label(req.uri().path());
    let bytes = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    state.registry.record_request(ip, endpoint, bytes);
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(n: u8) -> ClientIp {
        ClientIp::Addr(IpAddr::from([127, 0, 0, n]))
    }

    #[test]
    fn unresolved_ip_records_under_unknown() {
        let reg = IpTrafficRegistry::new();
        reg.record_request(ClientIp::Unknown, EP_NOTIFY, 42);
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].ip, "unknown");
        assert_eq!(
            snap[0].counts,
            IpCounts {
                requests: 1,
                bytes: 42
            }
        );
    }

    #[test]
    fn endpoint_label_maps_known_routes_else_other() {
        assert_eq!(endpoint_label("/content/join/node-1"), EP_CONTENT_JOIN);
        assert_eq!(endpoint_label("/content/host/dl-abc"), EP_CONTENT_HOST);
        assert_eq!(endpoint_label("/pair/host/rid"), EP_PAIR_HOST);
        assert_eq!(endpoint_label("/pair/join/rid"), EP_PAIR_JOIN);
        assert_eq!(endpoint_label("/control"), EP_CONTROL);
        assert_eq!(endpoint_label("/notify"), EP_NOTIFY);
        assert_eq!(endpoint_label("/register"), EP_REGISTER);
        assert_eq!(endpoint_label("/wat/ever"), EP_OTHER);
    }

    #[test]
    fn request_and_leg_bytes_accumulate_on_one_entry() {
        let reg = IpTrafficRegistry::new();
        // The middleware records a request + its body bytes...
        reg.record_request(ip(1), EP_CONTENT_JOIN, 0);
        // ...and the leg adds relayed bytes onto the same (ip, endpoint).
        let meter = reg.meter_for(ip(1), EP_CONTENT_JOIN);
        meter.record(500);
        meter.record(40);

        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1, "one shared (ip, endpoint) entry");
        assert_eq!(
            snap[0].counts,
            IpCounts {
                requests: 1,
                bytes: 540
            }
        );
    }

    #[test]
    fn distinct_ip_or_endpoint_are_separate() {
        let reg = IpTrafficRegistry::new();
        reg.record_request(ip(1), EP_NOTIFY, 100);
        reg.record_request(ip(1), EP_REGISTER, 200);
        reg.record_request(ip(2), EP_NOTIFY, 7);
        assert_eq!(reg.tracked_len(), 3);
    }

    #[test]
    fn collect_commit_collect_is_delta_since_last_commit() {
        let reg = IpTrafficRegistry::new();
        reg.record_request(ip(1), EP_NOTIFY, 10);
        let first = reg.collect();
        assert_eq!(first.len(), 1);
        assert_eq!(
            first[0].counts,
            IpCounts {
                requests: 1,
                bytes: 10
            }
        );
        reg.commit();
        assert!(reg.collect().is_empty(), "drained → no fresh delta");
        reg.record_request(ip(1), EP_NOTIFY, 5);
        let second = reg.collect();
        assert_eq!(
            second[0].counts,
            IpCounts {
                requests: 1,
                bytes: 5
            }
        );
    }

    #[test]
    fn eviction_drops_a_dead_entry_but_keeps_a_leg_held_one() {
        let reg = IpTrafficRegistry::with_limits(64, 1);
        // A request-only entry (no live leg handle): evictable once idle + drained.
        reg.record_request(ip(2), EP_NOTIFY, 1);
        // A leg-held entry: its IpByteMeter keeps the Arc alive (strong_count ≥ 2).
        let leg = reg.meter_for(ip(1), EP_CONTENT_JOIN);
        leg.record(1);
        assert_eq!(reg.tracked_len(), 2);

        reg.collect();
        reg.commit(); // interval 1: both had a delta, idle reset
        reg.collect();
        reg.commit(); // interval 2: no new traffic → dead one evicted
        assert_eq!(reg.tracked_len(), 1, "only the leg-held entry survives");
        assert_eq!(reg.snapshot()[0].endpoint, EP_CONTENT_JOIN);
    }

    #[test]
    fn at_cap_a_new_pair_is_inert() {
        let reg = IpTrafficRegistry::with_limits(1, 5);
        reg.record_request(ip(1), EP_NOTIFY, 5);
        // Map full → a new (ip, endpoint) records nothing and is not tracked.
        reg.record_request(ip(2), EP_NOTIFY, 9);
        assert!(reg.meter_for(ip(2), EP_CONTENT_JOIN).counters.is_none());
        assert_eq!(reg.tracked_len(), 1);
    }
}
