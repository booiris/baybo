//! The relay service: the blind pairing-rendezvous server (C).
//!
//! Two asymmetric routes ride the same [`RelayBroker`], keyed by the **public
//! `rendezvous_id`** (a UUID):
//!
//! - `GET /pair/host/{rendezvous_id}` — the **gateway** side. Authenticated by
//!   the gateway's `remote_api_key` ([`REMOTE_API_KEY_HEADER`]); on success it
//!   parks a leg under `rendezvous_id` and waits for the app.
//! - `GET /pair/join/{rendezvous_id}` — the **app** side. Also gated by an
//!   admitted `remote_api_key` ([`REMOTE_API_KEY_HEADER`], the one the QR carries),
//!   so only key-holders can use the relay's pairing rendezvous. It [`try_match`]es
//!   an already-hosted rendezvous and is refused if no admitted gateway hosts it.
//!
//! Both legs must present an admitted key, but only the gateway's host leg parks
//! and counts against the per-key connection cap; the ephemeral app leg
//! does not. C sees only the public `rendezvous_id` and copies opaque Noise
//! frames blind — the QR **secret** (the pairing handshake's PSK) never reaches
//! C, so a hostile relay cannot complete the handshake with either side (MITM is
//! reduced to denial-of-service).
//!
//! [`try_match`]: RelayBroker::try_match

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Extension, FromRequestParts, Path, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use remote_host_admission::{Admission, AdmissionEntry, Admit};
use remote_host_edge::ip_limit::{self, resolve_client_ip_from};
use remote_host_edge::ip_traffic::{ClientIp, EP_CONTENT_HOST, EP_CONTENT_JOIN};
use remote_host_protocol::relay::{
    CONTENT_HOST, CONTENT_JOIN, CONTROL, LegClass, PAIR_HOST, PAIR_JOIN, RELAY_LEG_CLASS_HEADER,
    REMOTE_API_KEY_HEADER,
};
// `build_router` (pub) takes an `IpLimitConfig` and a `RelayServices` carrying an
// `IpTrafficRegistry`, so downstream callers must be able to name both without
// depending on `remote-host-edge` directly.
pub use remote_host_edge::IpTrafficRegistry;
pub use remote_host_edge::ip_limit::IpLimitConfig;

use crate::bandwidth::{BandwidthRegistry, LegClass as BwClass};
use crate::broker::{ParkOutcome, RelayBroker};
use crate::conns::ConnectionRegistry;
use crate::control::{ControlHello, ControlRegisterError, ControlRegistry, SignalOpenError};
use crate::traffic::{Direction, LegMetering, TrafficRegistry};
use crate::ws::pump_ws;

/// Drop a gateway control connection that has gone silent for this long. The
/// gateway keepalive-Pings well inside it (every 30s), so only a half-open
/// connection trips it — releasing the stale registry slot promptly.
const CONTROL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Cap on a single relayed WebSocket message. Content frames are chunked Noise
/// messages (≤ ~64 KiB each) and pairing/control frames are far smaller, so this
/// never truncates legitimate traffic. It bounds per-frame memory and keeps the
/// bandwidth throttle fine-grained: a frame is read whole before it is throttled,
/// so without this cap axum's 64 MiB default would let one (buggy or hostile)
/// frame balloon memory and stall the pump for tens of seconds. An oversized
/// frame trips a WS protocol error and closes the leg instead.
const MAX_RELAY_FRAME_BYTES: usize = 128 * 1024;

/// Apply the relay's per-message size cap to a WS upgrade (every relay route
/// carries only small opaque frames).
fn capped(ws: WebSocketUpgrade) -> WebSocketUpgrade {
    ws.max_message_size(MAX_RELAY_FRAME_BYTES)
        .max_frame_size(MAX_RELAY_FRAME_BYTES)
}

/// `/pair/join` attempts allowed per rendezvous id within [`JOIN_WINDOW`].
/// Generous enough for the app's own connect-retry loop (≈30 attempts at 500ms
/// while it waits for the host leg to park), tight enough that a griefer who
/// learned the public rendezvous id can't flood it with leg-stealing joins.
const JOIN_MAX_PER_WINDOW: usize = 30;
/// Fixed window for the `/pair/join` rate limit.
const JOIN_WINDOW: Duration = Duration::from_secs(10);
/// Soft cap on tracked rendezvous ids before stale windows are pruned — bounds
/// memory against an attacker spraying distinct ids.
const JOIN_LIMITER_MAX_KEYS: usize = 4096;

/// Cap on phone-side content legs that have been signaled but not yet claimed by
/// the gateway's data leg. This bounds relay memory independently of the broker's
/// parked-leg cap because this map is the ownership/metering side table.
const MAX_PENDING_CONTENT_LEGS: usize = 1024;

/// A fixed-window count for one rendezvous id.
struct JoinWindow {
    count: usize,
    started: Instant,
}

/// Per-rendezvous-id rate limiter for `/pair/join`. Availability hardening only:
/// the PSK already defeats a hostile relay's MITM, but the *public* rendezvous id
/// lets any holder of it (or the shared admission key) grief pairing by stealing
/// the parked host leg, so we throttle joins per id.
#[derive(Default)]
struct JoinRateLimiter {
    windows: parking_lot::Mutex<HashMap<String, JoinWindow>>,
}

impl JoinRateLimiter {
    /// Record a join attempt for `rendezvous_id`; `false` if it is over the limit.
    fn allow(&self, rendezvous_id: &str) -> bool {
        let now = Instant::now();
        let mut windows = self.windows.lock();
        if windows.len() >= JOIN_LIMITER_MAX_KEYS {
            windows.retain(|_, w| now.duration_since(w.started) < JOIN_WINDOW);
        }
        let window = windows
            .entry(rendezvous_id.to_string())
            .or_insert(JoinWindow {
                count: 0,
                started: now,
            });
        if now.duration_since(window.started) >= JOIN_WINDOW {
            window.count = 0;
            window.started = now;
        }
        if window.count >= JOIN_MAX_PER_WINDOW {
            return false;
        }
        window.count += 1;
        true
    }
}

#[derive(Clone)]
struct PendingContentLeg {
    server_id: String,
    class: LegClass,
    owner_remote_api_key: String,
}

/// An optional socket-peer extractor that **never fails**: `Some` when the service
/// was served with `ConnectInfo<SocketAddr>` (production), `None` otherwise (e.g. a
/// test served without connect info). Lets the content handlers resolve a client IP
/// for byte accounting where available without rejecting the upgrade where it isn't
/// — unlike a bare `ConnectInfo` extractor, which `ConnectInfo` only implements as a
/// *fallible* `FromRequestParts`.
struct OptPeer(Option<SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for OptPeer {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(OptPeer(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ci| ci.0),
        ))
    }
}

#[derive(Clone)]
struct RelayState {
    broker: Arc<RelayBroker>,
    admitted: Arc<dyn Admission>,
    /// Live gateway control connections, keyed by `relay_node_id`.
    control: Arc<ControlRegistry>,
    /// Live admission-gated connections keyed by `remote_api_key`, so an admission
    /// hot-reload can drop the connections of a revoked key.
    conns: Arc<ConnectionRegistry>,
    /// Two-level content-bandwidth buckets (per-`remote_api_key` ceiling ∧
    /// per-`(key, server)` sub-cap); content legs throttle against the owning
    /// gateway's buckets (pairing legs are tiny and unthrottled).
    bandwidth: Arc<BandwidthRegistry>,
    /// Per-`(remote_api_key, server_id)` traffic counters; content legs record the
    /// bytes/frames they move (both directions of a session share one entry). The
    /// server's flush task drains it to the durable traffic ledger.
    traffic: Arc<TrafficRegistry>,
    /// Per-`(ip, endpoint)` traffic counters; content legs also attribute their
    /// relayed bytes to the source IP (phone IP for join, gateway IP for host).
    ip_traffic: Arc<IpTrafficRegistry>,
    /// Trusted proxy client-IP headers (from the IP-limit config), so the content
    /// handlers resolve the same client IP the limiter/recorder middleware do.
    trusted_headers: Arc<Vec<HeaderName>>,
    /// `relay_key` → content leg metadata awaiting the gateway host. The phone's
    /// content-join writes it (it knows the server, class, and owning admission
    /// key); the gateway's content-host must claim it with that same key before it
    /// can touch the broker. Removed when the host claims it, on signaling failure,
    /// or when the phone leg ends unclaimed.
    pending_content_legs: Arc<parking_lot::Mutex<HashMap<String, PendingContentLeg>>>,
    /// Per-rendezvous `/pair/join` throttle (anti-grief; see [`JoinRateLimiter`]).
    join_limiter: Arc<JoinRateLimiter>,
}

/// The shared registries a relay router meters against, passed to [`build_router`]
/// as a struct so every required dependency shows up by name at the call site.
pub struct RelayServices {
    /// Hot-reloaded allow-list of admitted `remote_api_key`s.
    pub admission: Arc<dyn Admission>,
    /// Live connections, so a revoke (an admission reload that dropped a key) can
    /// kick them.
    pub conns: Arc<ConnectionRegistry>,
    /// Two-level content-bandwidth throttle (per key ∧ per server).
    pub bandwidth: Arc<BandwidthRegistry>,
    /// Per-`(remote_api_key, server_id)` traffic ledger.
    pub traffic: Arc<TrafficRegistry>,
    /// Per-`(ip, endpoint)` traffic ledger (content legs add relayed bytes to it).
    pub ip_traffic: Arc<IpTrafficRegistry>,
}

/// Assemble the relay router from its shared [`RelayServices`].
///
/// `ip_limit` configures the shared per-source-IP upgrade throttle (see
/// [`remote_host_edge::ip_limit`]). Enable it keying on the socket peer when
/// remote-host terminates TLS itself (the peer is the real client); behind a
/// proxy, either disable it (rate-limit at the proxy) or give it the trusted
/// client-IP header(s) the proxy sets (e.g. `cf-connecting-ip`) — see the
/// [`IpLimitConfig`] trust caveat. The content handlers reuse those same trusted
/// headers to resolve each leg's client IP for the per-IP byte accounting.
pub fn build_router(services: RelayServices, ip_limit: IpLimitConfig) -> Router {
    let RelayServices {
        admission,
        conns,
        bandwidth,
        traffic,
        ip_traffic,
    } = services;
    let state = RelayState {
        broker: Arc::new(RelayBroker::new()),
        admitted: admission,
        control: Arc::new(ControlRegistry::new()),
        conns,
        bandwidth,
        traffic,
        ip_traffic,
        trusted_headers: Arc::new(ip_limit.trusted_headers.clone()),
        pending_content_legs: Arc::new(parking_lot::Mutex::new(HashMap::new())),
        join_limiter: Arc::new(JoinRateLimiter::default()),
    };
    // Every route admits via the shared `x-remote-api-key` pre-layer — including
    // `/control`, whose key now rides the dial header too (its hello carries only
    // the relay_node_id).
    let router = Router::new()
        .route(PAIR_HOST, get(host_handler))
        .route(PAIR_JOIN, get(join_handler))
        // Content relay (phase 2): the gateway holds a control connection; a
        // phone names the gateway by relay_node_id and C splices a data leg.
        .route(CONTROL, get(control_handler))
        .route(CONTENT_JOIN, get(content_join_handler))
        .route(CONTENT_HOST, get(content_host_handler))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_admitted,
        ))
        .with_state(state);
    // The shared per-IP limiter is added *after* admission so it wraps it as the
    // outermost layer (tower runs outer→inner), shedding a flood by client IP
    // before the admission check and any upgrade work.
    ip_limit::apply(router, ip_limit)
}

/// `401 Unauthorized` for a missing or unadmitted `remote_api_key`.
fn unadmitted() -> Response {
    (StatusCode::UNAUTHORIZED, "remote_api_key not admitted").into_response()
}

/// Map a wire [`LegClass`] to its relay bandwidth class: a `Blob` leg meters as
/// `Background` (yields the chat headroom on the shared bucket), a `Chat` leg as
/// `Interactive`.
fn bandwidth_class(class: LegClass) -> BwClass {
    match class {
        LegClass::Blob => BwClass::Background,
        LegClass::Chat => BwClass::Interactive,
    }
}

/// TOCTOU recheck used by the host handlers: a key admitted by the pre-layer may
/// have been revoked before the handler registered its connection.
fn still_admitted(admission: &dyn Admission, remote_api_key: &str) -> bool {
    matches!(admission.resolve(remote_api_key), Admit::Ok(_))
}

/// `503 Service Unavailable` when the broker's parked-leg ceiling is reached —
/// a transient global flood backstop, so the dialer can back off and retry.
fn at_capacity() -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, "relay at capacity").into_response()
}

fn new_content_relay_key() -> String {
    format!("dl-{}", hex::encode(rand::random::<[u8; 16]>()))
}

/// `409 Conflict` when a host leg targets a rendezvous another host already holds.
/// Refusing (rather than splicing the two hosts) is the structural guard; the
/// gateway re-hosts after a short backoff, by when the prior host's leg has been
/// cancelled or TTL-swept.
fn host_occupied() -> Response {
    (StatusCode::CONFLICT, "rendezvous already hosted").into_response()
}

/// The admitted `remote_api_key` for a request plus its resolved limits, stashed by
/// [`require_admitted`] so the handlers that meter on it (the connection cap /
/// bandwidth buckets) can read it back as an `Extension` without re-resolving.
#[derive(Clone)]
struct Admitted {
    remote_api_key: String,
    entry: AdmissionEntry,
}

/// Admission pre-layer for the header-gated routes: `401` unless `x-remote-api-key`
/// resolves to an admitted, unexpired key, then stash the key + its limits as an
/// [`Admitted`] extension. `/control` admits via this same layer (its key rides the
/// dial header; the WS hello carries only the `relay_node_id`).
async fn require_admitted(
    State(state): State<RelayState>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(key) = req
        .headers()
        .get(REMOTE_API_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned())
    else {
        return unadmitted();
    };
    match state.admitted.resolve(&key) {
        Admit::Ok(entry) => {
            req.extensions_mut().insert(Admitted {
                remote_api_key: key,
                entry,
            });
            next.run(req).await
        }
        Admit::Unknown | Admit::Expired => unadmitted(),
    }
}

/// Gateway side: the admission pre-layer authenticates the `remote_api_key`, then
/// we park a leg under `rendezvous_id`.
async fn host_handler(
    Path(rendezvous_id): Path<String>,
    State(state): State<RelayState>,
    Extension(Admitted {
        remote_api_key: key,
        entry,
    }): Extension<Admitted>,
    ws: WebSocketUpgrade,
) -> Response {
    // Cap how many connections one remote_api_key may hold (control + legs).
    let Some((guard, kick)) = state
        .conns
        .register(&key, entry.max_conns.map(|c| c as usize))
    else {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "connection limit reached for this remote_api_key",
        )
            .into_response();
    };
    // Close the TOCTOU window: if the key was revoked between the admission check
    // above and registering, a concurrent kick may have already passed us by, and
    // future polls won't re-target an already-removed key — so re-check and abort
    // rather than linger un-kicked. (`guard` drops here, deregistering.)
    if !still_admitted(state.admitted.as_ref(), &key) {
        return unadmitted();
    }
    // Park-only: the host never matches an existing half, so two host legs on the
    // same (public) rendezvous id can't be spliced to each other — a second one is
    // refused `409` and its `guard` drops here, freeing the per-key slot.
    let leg = match state.broker.park(&rendezvous_id) {
        ParkOutcome::Parked(leg) => leg,
        ParkOutcome::Occupied => return host_occupied(),
        ParkOutcome::AtCapacity => return at_capacity(),
    };
    let broker = Arc::clone(&state.broker);
    capped(ws).on_upgrade(move |socket| async move {
        let _guard = guard;
        tokio::select! {
            // Pairing frames are tiny opaque Noise blobs — not bandwidth-throttled.
            _ = pump_ws(socket, leg, None) => {}
            // The gateway's remote_api_key was revoked mid-connection.
            _ = kick => {}
        }
        // If the app never matched (the host disconnected first), drop the
        // still-parked leg so a stale rendezvous can't linger. Only one host half
        // ever exists under a rendezvous id (`park` refuses a second), so this
        // removes our own half, never a newer host's.
        broker.cancel(&rendezvous_id);
    })
}

/// App side: match an already-hosted rendezvous; never park. Symmetric admission
/// requires the app to present the admitted key the QR carries. The phone leg is
/// ephemeral, so unlike the host leg it is not registered against the gateway's
/// per-key connection cap.
async fn join_handler(
    Path(rendezvous_id): Path<String>,
    State(state): State<RelayState>,
    ws: WebSocketUpgrade,
) -> Response {
    // The rendezvous id is public (the relay routes on it), so anyone who learns
    // it — including a non-relay holder of the shared admission key — could
    // repeatedly steal the gateway's parked host leg and fail the PSK handshake
    // to grief pairing. Rate-limit joins per rendezvous id so a griefer can't
    // hammer one rendezvous. (Availability only; the PSK already blocks MITM.)
    if !state.join_limiter.allow(&rendezvous_id) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "too many pairing join attempts for this rendezvous",
        )
            .into_response();
    }
    match state.broker.try_match(&rendezvous_id) {
        Some(leg) => capped(ws).on_upgrade(move |socket| pump_ws(socket, leg, None)),
        None => (StatusCode::NOT_FOUND, "no pairing host for this rendezvous").into_response(),
    }
}

/// Gateway control connection. The admission pre-layer authenticates the
/// `x-remote-api-key` header; the gateway then holds the WS open, naming itself
/// with a [`ControlHello`] (`relay_node_id`). C pushes `ControlSignal`s ("a phone
/// arrived, open a data leg") over it.
async fn control_handler(
    State(state): State<RelayState>,
    Extension(Admitted {
        remote_api_key: key,
        ..
    }): Extension<Admitted>,
    ws: WebSocketUpgrade,
) -> Response {
    capped(ws).on_upgrade(move |socket| run_control(socket, state, key))
}

async fn run_control(mut socket: WebSocket, state: RelayState, key: String) {
    let hello = match socket.recv().await {
        Some(Ok(AxumMessage::Binary(b))) => match serde_json::from_slice::<ControlHello>(&b) {
            Ok(h) => h,
            Err(e) => {
                tracing::debug!(error = %e, "control: unparseable hello; closing");
                return;
            }
        },
        _ => return,
    };
    tracing::info!(node = %hello.relay_node_id, "control: gateway connected");
    // The control connection is exempt from the per-key cap (essential, ~one per
    // gateway) so a gateway at its leg limit can still (re)establish control.
    let (_kick_guard, mut kick) = state.conns.register_unchecked(&key);
    // Close the TOCTOU window (see the host handlers): the pre-layer admitted the
    // key, but a concurrent revoke's kick may have run before we registered.
    if !still_admitted(state.admitted.as_ref(), &key) {
        return;
    }
    let (mut rx, control_token) = match state.control.register(&hello.relay_node_id, &key) {
        Ok(registered) => registered,
        Err(ControlRegisterError::OwnerMismatch) => {
            tracing::warn!(
                node = %hello.relay_node_id,
                "control: relay_node_id already registered to a different remote_api_key"
            );
            return;
        }
    };
    loop {
        tokio::select! {
            // The gateway's remote_api_key was revoked (admission hot-reload).
            _ = &mut kick => {
                tracing::info!(node = %hello.relay_node_id, "control: remote_api_key revoked; closing");
                break;
            }
            sig = rx.recv() => match sig {
                Some(signal) => {
                    let json = match serde_json::to_vec(&signal) {
                        Ok(j) => j,
                        Err(_) => continue,
                    };
                    if socket.send(AxumMessage::Binary(json.into())).await.is_err() {
                        break;
                    }
                }
                // Our slot was superseded by a reconnect (the prior tx was
                // dropped, closing `rx`). The token-scoped unregister below is a
                // no-op against the new owner, so just exit.
                None => break,
            },
            inbound = socket.recv() => match inbound {
                // The gateway speaks only the hello; anything else (its keepalive
                // Pings, auto-Ponged by axum) just proves liveness.
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            // The gateway pings well within this window; silence past it means a
            // half-open connection, so drop it (and unregister) rather than pin a
            // dead slot until the OS TCP timeout.
            _ = tokio::time::sleep(CONTROL_IDLE_TIMEOUT) => {
                tracing::debug!(node = %hello.relay_node_id, "control: idle timeout; closing");
                break;
            }
        }
    }
    state
        .control
        .unregister_if_owned(&hello.relay_node_id, control_token);
    tracing::info!(node = %hello.relay_node_id, "control: gateway disconnected");
}

/// App side of a content session: the phone names the gateway by `relay_node_id`.
/// C signals that gateway (over its control connection) to open a data leg under
/// a fresh `relay_key`, then joins the phone's leg to it — blind. Refused fast
/// if no gateway is currently connected for that id.
async fn content_join_handler(
    Path(relay_node_id): Path<String>,
    State(state): State<RelayState>,
    Extension(Admitted {
        remote_api_key: phone_key,
        ..
    }): Extension<Admitted>,
    headers: HeaderMap,
    OptPeer(peer): OptPeer,
    ws: WebSocketUpgrade,
) -> Response {
    // The phone authors the leg class on its own join (the relay copies it, never
    // authors it). Absent/unparseable ⇒ Chat — a bad hint must never fail a join.
    let class = headers
        .get(RELAY_LEG_CLASS_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(LegClass::from_header_value)
        .unwrap_or_default();
    let relay_key = loop {
        let candidate = new_content_relay_key();
        let mut pending = state.pending_content_legs.lock();
        if pending.len() >= MAX_PENDING_CONTENT_LEGS {
            return at_capacity();
        }
        if !pending.contains_key(&candidate) {
            pending.insert(
                candidate.clone(),
                PendingContentLeg {
                    server_id: relay_node_id.clone(),
                    class,
                    owner_remote_api_key: phone_key.clone(),
                },
            );
            break candidate;
        }
    };
    // Record this leg's server (the relay_node_id) and class so the gateway's
    // content-host — whose dial carries only the relay_key — can meter against the
    // right per-server bucket *and* class. Inserted *before* signaling, since the
    // signal is what makes the gateway dial host (which could otherwise race ahead
    // of this write).
    // The phone leg is admitted by the pre-layer like every other route, but
    // metering keys on the *gateway's* remote_api_key (resolved by signaling) and
    // its server, so both legs of a content session share one bucket pair.
    let remote_api_key =
        match state
            .control
            .signal_open(&relay_node_id, &phone_key, &relay_key, class)
        {
            Ok(key) => key,
            Err(SignalOpenError::NotConnected) => {
                state.pending_content_legs.lock().remove(&relay_key);
                return (StatusCode::SERVICE_UNAVAILABLE, "gateway not connected").into_response();
            }
            Err(SignalOpenError::OwnerMismatch) => {
                state.pending_content_legs.lock().remove(&relay_key);
                return (
                    StatusCode::FORBIDDEN,
                    "relay node belongs to a different remote_api_key",
                )
                    .into_response();
            }
            Err(SignalOpenError::Backpressure) => {
                state.pending_content_legs.lock().remove(&relay_key);
                return (StatusCode::SERVICE_UNAVAILABLE, "gateway control busy").into_response();
            }
        };
    // Pull the gateway key's limits; if it was revoked since it registered control,
    // fall back to the role floor (the conn kick tears the session down anyway).
    let (max_bps, per_server_max_bps) = match state.admitted.resolve(&remote_api_key) {
        Admit::Ok(e) => (e.max_bps, e.per_server_max_bps),
        Admit::Unknown | Admit::Expired => (None, None),
    };
    let limiter = state
        .bandwidth
        .limiter_for(&remote_api_key, &relay_node_id, max_bps, per_server_max_bps)
        .with_class(bandwidth_class(class));
    let Some(leg) = state.broker.join(&relay_key) else {
        state.pending_content_legs.lock().remove(&relay_key);
        return at_capacity();
    };
    // Phone leg: its inbound is the uplink (phone → gateway). Meters against the
    // gateway's (key, server) so up + down share one entry. Resolved after the join
    // succeeds, so a doomed join never populates the traffic map.
    let meter = state
        .traffic
        .meter_for(&remote_api_key, &relay_node_id, Direction::Up);
    // Attribute this leg's relayed bytes to the phone's source IP — the same client
    // IP the recorder middleware counted the request under.
    let ip_meter = state.ip_traffic.meter_for(
        ClientIp::from_opt(resolve_client_ip_from(
            &headers,
            peer.map(|p| p.ip()),
            &state.trusted_headers,
        )),
        EP_CONTENT_JOIN,
    );
    let broker = Arc::clone(&state.broker);
    let pending = Arc::clone(&state.pending_content_legs);
    capped(ws).on_upgrade(move |socket| async move {
        pump_ws(
            socket,
            leg,
            Some(LegMetering {
                limiter,
                meter,
                ip_meter,
            }),
        )
        .await;
        broker.cancel(&relay_key);
        // Backstop: if the gateway host never claimed this leg, drop the mapping so
        // it can't linger after the phone leg ends.
        pending.lock().remove(&relay_key);
    })
}

/// Gateway side of a content session: the gateway, signaled over its control
/// connection, opens this leg under the C-issued `relay_key` and is matched to
/// the waiting phone. Admission-gated like the pairing host so only the real
/// gateway can occupy the leg.
async fn content_host_handler(
    Path(relay_key): Path<String>,
    State(state): State<RelayState>,
    Extension(Admitted {
        remote_api_key: key,
        entry,
    }): Extension<Admitted>,
    headers: HeaderMap,
    OptPeer(peer): OptPeer,
    ws: WebSocketUpgrade,
) -> Response {
    // Recover the phone-side metadata for this relay key. This is the ownership
    // gate: only the gateway registered under the same remote_api_key may claim
    // the pending leg, and an unknown relay key must not be allowed to pre-park in
    // the broker.
    let pending_leg = {
        let mut pending = state.pending_content_legs.lock();
        let Some(existing) = pending.get(&relay_key) else {
            return (StatusCode::NOT_FOUND, "no pending content leg").into_response();
        };
        if existing.owner_remote_api_key != key {
            return (
                StatusCode::FORBIDDEN,
                "wrong remote_api_key for content leg",
            )
                .into_response();
        }
        let Some(pending_leg) = pending.remove(&relay_key) else {
            return (StatusCode::NOT_FOUND, "no pending content leg").into_response();
        };
        pending_leg
    };
    let server_id = pending_leg.server_id;
    let class = pending_leg.class;
    // Cap how many connections one remote_api_key may hold (control + legs). A blob
    // leg is held below the chat reserve so a blob flood can't refuse a chat leg.
    let max_conns = entry.max_conns.map(|c| c as usize);
    let registered = match class {
        LegClass::Blob => state.conns.register_background(&key, max_conns),
        LegClass::Chat => state.conns.register(&key, max_conns),
    };
    let Some((guard, kick)) = registered else {
        state.broker.cancel(&relay_key);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "connection limit reached for this remote_api_key",
        )
            .into_response();
    };
    // Close the TOCTOU window: if the key was revoked between the admission check
    // above and registering, a concurrent kick may have already passed us by, and
    // future polls won't re-target an already-removed key — so re-check and abort
    // rather than linger un-kicked. (`guard` drops here, deregistering.)
    if !still_admitted(state.admitted.as_ref(), &key) {
        state.broker.cancel(&relay_key);
        return unadmitted();
    }
    let limiter = state
        .bandwidth
        .limiter_for(&key, &server_id, entry.max_bps, entry.per_server_max_bps)
        .with_class(bandwidth_class(class));
    let Some(leg) = state.broker.join(&relay_key) else {
        state.broker.cancel(&relay_key);
        return at_capacity();
    };
    // Gateway leg: its inbound is the downlink (gateway → phone). Same (key, server)
    // as the phone leg, so it accumulates into the same entry's down-counters.
    // Resolved after the join succeeds, so a doomed join never populates the map.
    let meter = state.traffic.meter_for(&key, &server_id, Direction::Down);
    // Attribute this leg's relayed bytes to the gateway's source IP.
    let ip_meter = state.ip_traffic.meter_for(
        ClientIp::from_opt(resolve_client_ip_from(
            &headers,
            peer.map(|p| p.ip()),
            &state.trusted_headers,
        )),
        EP_CONTENT_HOST,
    );
    let broker = Arc::clone(&state.broker);
    capped(ws).on_upgrade(move |socket| async move {
        let _guard = guard;
        tokio::select! {
            _ = pump_ws(
                socket,
                leg,
                Some(LegMetering {
                    limiter,
                    meter,
                    ip_meter,
                }),
            ) => {}
            // The gateway's remote_api_key was revoked mid-session.
            _ = kick => {}
        }
        broker.cancel(&relay_key);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderName;
    use futures::{SinkExt, StreamExt};
    use std::net::SocketAddr;
    use tokio::net::TcpStream;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::StatusCode as WsStatus;
    use tokio_tungstenite::tungstenite::{Error as WsError, Message};
    use tokio_tungstenite::{WebSocketStream, client_async};

    async fn serve() -> u16 {
        let admission = Arc::new(remote_host_admission::InMemoryAdmission::with_keys([
            "inst-A",
        ]));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = build_router(
            RelayServices {
                admission,
                conns: Arc::new(ConnectionRegistry::new()),
                bandwidth: Arc::new(BandwidthRegistry::new()),
                traffic: Arc::new(TrafficRegistry::new()),
                ip_traffic: Arc::new(IpTrafficRegistry::new()),
            },
            IpLimitConfig::socket_peer(),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        port
    }

    /// Serve with the client socket address attached (as production does), so the
    /// per-IP limiter middleware sees a peer address. The plain [`serve`] omits it
    /// — that path proves the limiter is skipped (not 500s) without connect info.
    async fn serve_with_connect_info() -> u16 {
        serve_with_ip_limit(IpLimitConfig::socket_peer()).await
    }

    /// Like [`serve_with_connect_info`] but with an explicit limiter config, so a
    /// test can exercise trusted-header client-IP resolution.
    async fn serve_with_ip_limit(ip_limit: IpLimitConfig) -> u16 {
        let admission = Arc::new(remote_host_admission::InMemoryAdmission::with_keys([
            "inst-A",
        ]));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = build_router(
            RelayServices {
                admission,
                conns: Arc::new(ConnectionRegistry::new()),
                bandwidth: Arc::new(BandwidthRegistry::new()),
                traffic: Arc::new(TrafficRegistry::new()),
                ip_traffic: Arc::new(IpTrafficRegistry::new()),
            },
            ip_limit,
        );
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });
        port
    }

    /// One keyless `/pair/host` upgrade attempt; returns the HTTP status the
    /// server rejected it with (it never completes the WS handshake).
    async fn host_attempt_status(port: u16) -> WsStatus {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let req = format!("ws://127.0.0.1:{port}/pair/host/x")
            .into_client_request()
            .unwrap();
        match client_async(req, stream).await {
            Err(WsError::Http(resp)) => resp.status(),
            other => panic!("expected an HTTP rejection, got {other:?}"),
        }
    }

    /// Like [`host_attempt_status`] but carrying a `CF-Connecting-IP` header (when
    /// `cf_ip` is `Some`), so a test can drive the trusted-header IP resolution
    /// over a single loopback socket.
    async fn host_attempt_with_cf_ip(port: u16, cf_ip: Option<&str>) -> WsStatus {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut req = format!("ws://127.0.0.1:{port}/pair/host/x")
            .into_client_request()
            .unwrap();
        if let Some(ip) = cf_ip {
            req.headers_mut()
                .insert("cf-connecting-ip", ip.parse().unwrap());
        }
        match client_async(req, stream).await {
            Err(WsError::Http(resp)) => resp.status(),
            other => panic!("expected an HTTP rejection, got {other:?}"),
        }
    }

    async fn connect_host(port: u16, code: &str, key: Option<&str>) -> WebSocketStream<TcpStream> {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut req = format!("ws://127.0.0.1:{port}/pair/host/{code}")
            .into_client_request()
            .unwrap();
        if let Some(k) = key {
            req.headers_mut()
                .insert(REMOTE_API_KEY_HEADER, k.parse().unwrap());
        }
        client_async(req, stream).await.unwrap().0
    }

    async fn connect_join(
        port: u16,
        code: &str,
        key: Option<&str>,
    ) -> Result<WebSocketStream<TcpStream>, WsError> {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut req = format!("ws://127.0.0.1:{port}/pair/join/{code}")
            .into_client_request()
            .unwrap();
        if let Some(k) = key {
            req.headers_mut()
                .insert(REMOTE_API_KEY_HEADER, k.parse().unwrap());
        }
        Ok(client_async(req, stream).await?.0)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn admitted_host_and_app_rendezvous_blind() {
        let port = serve().await;
        // The host parks synchronously inside the handler before its 101, so by
        // the time `connect_host` returns the leg is already waiting to match.
        let mut host = connect_host(port, "CODE1", Some("inst-A")).await;
        let mut app = connect_join(port, "CODE1", Some("inst-A"))
            .await
            .expect("host is parked");

        host.send(Message::Binary(b"a->p".to_vec())).await.unwrap();
        assert_eq!(recv_bin(&mut app).await, b"a->p");
        app.send(Message::Binary(b"p->a".to_vec())).await.unwrap();
        assert_eq!(recv_bin(&mut host).await, b"p->a");
    }

    /// A frame past the per-message cap trips a WS protocol error: the server
    /// closes the leg instead of buffering the whole oversized frame (which would
    /// balloon memory and stall the throttle).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oversized_frame_closes_the_leg() {
        let port = serve().await;
        // Hold the host leg so the app's join matches; it's torn down at the end.
        let host = connect_host(port, "BIG", Some("inst-A")).await;
        let mut app = connect_join(port, "BIG", Some("inst-A"))
            .await
            .expect("host is parked");

        // One byte over the cap. The client write may flush locally; the server
        // rejects it on read and tears the connection down.
        let huge = vec![0u8; MAX_RELAY_FRAME_BYTES + 1];
        let _ = app.send(Message::Binary(huge)).await;

        // The app's leg closes (Close frame or EOF/error), and the host never
        // receives the oversized payload.
        let closed = loop {
            match app.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break true,
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                Some(Ok(_)) => break false,
            }
        };
        assert!(closed, "server closes the leg on an oversized frame");
        drop(host);
    }

    /// Two host legs on the same rendezvous: the second is refused `409` (the
    /// park-only guard), never spliced to the first — and the first stays parked
    /// and matchable by the app.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_host_on_a_rendezvous_is_refused_409() {
        let port = serve().await;
        // The first host parks synchronously inside the handler before its 101.
        let mut host1 = connect_host(port, "RID", Some("inst-A")).await;

        // A second host on the same id is refused with 409 — not matched to host1.
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut req = format!("ws://127.0.0.1:{port}/pair/host/RID")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert(REMOTE_API_KEY_HEADER, "inst-A".parse().unwrap());
        match client_async(req, stream).await {
            Err(WsError::Http(resp)) => assert_eq!(resp.status(), WsStatus::CONFLICT),
            other => panic!("expected 409 for a second host, got {other:?}"),
        }

        // host1 is intact: the app matches it and bytes flow.
        let mut app = connect_join(port, "RID", Some("inst-A"))
            .await
            .expect("app matches the still-parked first host");
        host1.send(Message::Binary(b"a->p".to_vec())).await.unwrap();
        assert_eq!(recv_bin(&mut app).await, b"a->p");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unadmitted_host_is_rejected() {
        let port = serve().await;
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let req = format!("ws://127.0.0.1:{port}/pair/host/CODE2")
            .into_client_request()
            .unwrap();
        // No remote-api-key header → the upgrade is refused with 401.
        match client_async(req, stream).await {
            Err(WsError::Http(resp)) => assert_eq!(resp.status(), WsStatus::UNAUTHORIZED),
            other => panic!("expected 401, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn join_without_host_is_refused() {
        let port = serve().await;
        match connect_join(port, "NOHOST", Some("inst-A")).await {
            Err(WsError::Http(resp)) => assert_eq!(resp.status(), WsStatus::NOT_FOUND),
            other => panic!("expected 404, got {other:?}"),
        }
    }

    #[test]
    fn join_rate_limiter_throttles_one_rendezvous_then_resets() {
        let limiter = JoinRateLimiter::default();
        // The first JOIN_MAX_PER_WINDOW attempts pass; the next is throttled.
        for i in 0..JOIN_MAX_PER_WINDOW {
            assert!(limiter.allow("rid-1"), "attempt {i} within the limit");
        }
        assert!(!limiter.allow("rid-1"), "over the limit is throttled");
        // A different rendezvous id has its own independent budget.
        assert!(limiter.allow("rid-2"), "distinct rendezvous unaffected");
        // Forcing the window start into the past resets the count.
        {
            let mut w = limiter.windows.lock();
            if let Some(win) = w.get_mut("rid-1") {
                win.started = Instant::now() - JOIN_WINDOW - Duration::from_secs(1);
            }
        }
        assert!(limiter.allow("rid-1"), "a fresh window admits again");
    }

    /// A flood of upgrade attempts from one source IP is shed with `429` once the
    /// per-IP burst is spent — and the limiter runs *ahead* of admission, so even
    /// keyless attempts (otherwise `401`) are throttled. Served *with* connect
    /// info so the middleware sees a peer address.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flood_from_one_ip_is_throttled_with_429() {
        let port = serve_with_connect_info().await;
        // Each keyless attempt draws one token then 401s; once the burst is spent
        // the limiter (outermost) 429s before admission even runs.
        let mut saw_401 = false;
        let mut saw_429 = false;
        for _ in 0..(remote_host_edge::ip_limit::IP_BURST as usize + 8) {
            match host_attempt_status(port).await {
                WsStatus::UNAUTHORIZED => saw_401 = true,
                WsStatus::TOO_MANY_REQUESTS => {
                    saw_429 = true;
                    break;
                }
                other => panic!("unexpected status during flood: {other}"),
            }
        }
        assert!(
            saw_401,
            "early attempts pass the limiter and 401 at admission"
        );
        assert!(
            saw_429,
            "the burst is eventually exhausted and 429'd by source IP"
        );
    }

    /// With a trusted `cf-connecting-ip` header configured, the limiter buckets by
    /// the **header** value, not the (shared) socket peer — so a flood from one CF
    /// client IP doesn't throttle a different CF client IP, and a request without
    /// the header falls back to the socket peer's own bucket. All over one loopback
    /// socket, proving the header (not the peer) is the key.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn per_ip_limit_keys_on_the_trusted_cf_header() {
        let port = serve_with_ip_limit(IpLimitConfig::with_trusted_headers(vec![
            HeaderName::from_static("cf-connecting-ip"),
        ]))
        .await;

        // Flood one CF client IP until it is throttled.
        let mut saw_429 = false;
        for _ in 0..(remote_host_edge::ip_limit::IP_BURST as usize + 8) {
            match host_attempt_with_cf_ip(port, Some("203.0.113.1")).await {
                WsStatus::UNAUTHORIZED => {}
                WsStatus::TOO_MANY_REQUESTS => {
                    saw_429 = true;
                    break;
                }
                other => panic!("unexpected status: {other}"),
            }
        }
        assert!(saw_429, "a flood from one CF client IP is throttled");

        // A *different* CF client IP — same socket — has its own bucket.
        assert_eq!(
            host_attempt_with_cf_ip(port, Some("203.0.113.2")).await,
            WsStatus::UNAUTHORIZED,
            "a distinct CF client IP is bucketed independently of the flooded one",
        );

        // No header → fall back to the socket peer (127.0.0.1), its own bucket.
        assert_eq!(
            host_attempt_with_cf_ip(port, None).await,
            WsStatus::UNAUTHORIZED,
            "without the trusted header the limiter falls back to the socket peer",
        );
    }

    /// Symmetric admission: the app's join leg also needs an admitted key — a
    /// keyless join is 401'd even when a gateway is hosting the rendezvous.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn join_without_an_admitted_key_is_rejected() {
        let port = serve().await;
        let _host = connect_host(port, "CODEK", Some("inst-A")).await;
        match connect_join(port, "CODEK", None).await {
            Err(WsError::Http(resp)) => assert_eq!(resp.status(), WsStatus::UNAUTHORIZED),
            other => panic!("expected 401, got {other:?}"),
        }
    }

    async fn connect_control(
        port: u16,
        key: Option<&str>,
    ) -> Result<WebSocketStream<TcpStream>, WsError> {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut req = format!("ws://127.0.0.1:{port}/control")
            .into_client_request()
            .unwrap();
        if let Some(k) = key {
            req.headers_mut()
                .insert(REMOTE_API_KEY_HEADER, k.parse().unwrap());
        }
        Ok(client_async(req, stream).await?.0)
    }

    async fn connect_content_join(
        port: u16,
        node: &str,
        key: Option<&str>,
    ) -> Result<WebSocketStream<TcpStream>, WsError> {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut req = format!("ws://127.0.0.1:{port}/content/join/{node}")
            .into_client_request()
            .unwrap();
        if let Some(k) = key {
            req.headers_mut()
                .insert(REMOTE_API_KEY_HEADER, k.parse().unwrap());
        }
        Ok(client_async(req, stream).await?.0)
    }

    async fn connect_content_host(
        port: u16,
        key: &str,
        remote_api_key: &str,
    ) -> WebSocketStream<TcpStream> {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut req = format!("ws://127.0.0.1:{port}/content/host/{key}")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert(REMOTE_API_KEY_HEADER, remote_api_key.parse().unwrap());
        client_async(req, stream).await.unwrap().0
    }

    async fn content_host_attempt_status(port: u16, key: &str, remote_api_key: &str) -> WsStatus {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut req = format!("ws://127.0.0.1:{port}/content/host/{key}")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert(REMOTE_API_KEY_HEADER, remote_api_key.parse().unwrap());
        match client_async(req, stream).await {
            Err(WsError::Http(resp)) => resp.status(),
            other => panic!("expected an HTTP rejection, got {other:?}"),
        }
    }

    async fn recv_control_json(ws: &mut WebSocketStream<TcpStream>) -> serde_json::Value {
        loop {
            match ws.next().await.unwrap().unwrap() {
                Message::Binary(b) => return serde_json::from_slice(&b).unwrap(),
                Message::Ping(_) | Message::Pong(_) => continue,
                other => panic!("unexpected control frame: {other:?}"),
            }
        }
    }

    /// Full content path: an admitted gateway holds a control connection; a
    /// phone names it by relay_node_id; C signals the gateway, which opens a data
    /// leg; the two legs splice and opaque bytes flow blind both ways.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn content_relay_splices_phone_and_gateway() {
        let port = serve().await;

        // Gateway holds its control connection (admitted) under node-1.
        let mut control = connect_control(port, Some("inst-A")).await.unwrap();
        let hello = serde_json::json!({ "relay_node_id": "node-1" });
        control
            .send(Message::Binary(serde_json::to_vec(&hello).unwrap()))
            .await
            .unwrap();

        // The phone dials content/join. Control registration is async, so retry
        // briefly until C admits the join (the gateway is registered).
        let mut app = None;
        for _ in 0..40 {
            match connect_content_join(port, "node-1", Some("inst-A")).await {
                Ok(ws) => {
                    app = Some(ws);
                    break;
                }
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
            }
        }
        let mut app = app.expect("content/join admitted once the gateway control registers");

        // C signaled the gateway to open a data leg; read the relay_key.
        let signal = recv_control_json(&mut control).await;
        assert_eq!(signal["t"], "open_data_leg");
        let relay_key = signal["relay_key"].as_str().unwrap().to_owned();

        // The gateway opens the data leg; the legs splice, bytes flow blind.
        let mut gw = connect_content_host(port, &relay_key, "inst-A").await;
        app.send(Message::Binary(b"noise-up".to_vec()))
            .await
            .unwrap();
        assert_eq!(recv_bin(&mut gw).await, b"noise-up");
        gw.send(Message::Binary(b"noise-down".to_vec()))
            .await
            .unwrap();
        assert_eq!(recv_bin(&mut app).await, b"noise-down");
    }

    /// The content relay records per-`(remote_api_key, server_id)` up/down traffic:
    /// after bytes flow both ways, the shared traffic entry shows the uplink (phone
    /// leg) and downlink (gateway leg) byte + frame counts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn content_relay_records_up_and_down_traffic() {
        let admission = Arc::new(remote_host_admission::InMemoryAdmission::with_keys([
            "inst-A",
        ]));
        let traffic = Arc::new(TrafficRegistry::new());
        let ip_traffic = Arc::new(IpTrafficRegistry::new());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = build_router(
            RelayServices {
                admission,
                conns: Arc::new(ConnectionRegistry::new()),
                bandwidth: Arc::new(BandwidthRegistry::new()),
                traffic: traffic.clone(),
                ip_traffic: ip_traffic.clone(),
            },
            IpLimitConfig::socket_peer(),
        );
        tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });

        // Gateway holds its control connection (admitted) under node-1.
        let mut control = connect_control(port, Some("inst-A")).await.unwrap();
        let hello = serde_json::json!({ "relay_node_id": "node-1" });
        control
            .send(Message::Binary(serde_json::to_vec(&hello).unwrap()))
            .await
            .unwrap();

        // Phone dials content/join; retry until the gateway control registers.
        let mut phone = None;
        for _ in 0..40 {
            match connect_content_join(port, "node-1", Some("inst-A")).await {
                Ok(ws) => {
                    phone = Some(ws);
                    break;
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(25)).await,
            }
        }
        let mut phone = phone.expect("content/join admitted once control registers");

        let signal = recv_control_json(&mut control).await;
        let relay_key = signal["relay_key"].as_str().unwrap().to_owned();
        let mut gw = connect_content_host(port, &relay_key, "inst-A").await;

        // phone -> gateway (uplink) and gateway -> phone (downlink).
        phone
            .send(Message::Binary(b"noise-up".to_vec()))
            .await
            .unwrap();
        assert_eq!(recv_bin(&mut gw).await, b"noise-up");
        gw.send(Message::Binary(b"down".to_vec())).await.unwrap();
        assert_eq!(recv_bin(&mut phone).await, b"down");

        // Recording happens on socket read, so poll briefly for both directions to
        // land on the shared (inst-A, node-1) entry.
        let mut recorded = None;
        for _ in 0..40 {
            let entry = traffic
                .snapshot()
                .into_iter()
                .find(|e| e.remote_api_key == "inst-A" && e.server_id == "node-1");
            if let Some(e) = entry
                && e.counts.bytes_up > 0
                && e.counts.bytes_down > 0
            {
                recorded = Some(e);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let e = recorded.expect("traffic recorded for (inst-A, node-1)");
        assert_eq!(e.counts.bytes_up, b"noise-up".len() as u64);
        assert_eq!(e.counts.frames_up, 1);
        assert_eq!(e.counts.bytes_down, b"down".len() as u64);
        assert_eq!(e.counts.frames_down, 1);

        // The same relayed bytes are attributed to the loopback source IP per
        // endpoint: the phone leg's uplink under content/join, the gateway leg's
        // downlink under content/host. (No request count here — build_router does
        // not mount the recorder middleware; that is main.rs's outer layer.)
        let ip_snap = ip_traffic.snapshot();
        let join = ip_snap
            .iter()
            .find(|e| e.ip == "127.0.0.1" && e.endpoint == "content/join")
            .expect("per-IP bytes for content/join");
        assert_eq!(join.counts.bytes, b"noise-up".len() as u64);
        let host = ip_snap
            .iter()
            .find(|e| e.ip == "127.0.0.1" && e.endpoint == "content/host")
            .expect("per-IP bytes for content/host");
        assert_eq!(host.counts.bytes, b"down".len() as u64);
    }

    /// A phone whose gateway has no live control connection is refused fast.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn content_join_without_a_connected_gateway_is_refused() {
        let port = serve().await;
        match connect_content_join(port, "ghost-node", Some("inst-A")).await {
            Err(WsError::Http(resp)) => assert_eq!(resp.status(), WsStatus::SERVICE_UNAVAILABLE),
            other => panic!("expected 503, got {other:?}"),
        }
    }

    /// A gateway content-host leg must claim a C-issued pending relay key.
    /// Unknown keys are rejected instead of parking a broker half that a future
    /// phone leg could be spliced into.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn content_host_unknown_relay_key_is_refused() {
        let port = serve().await;
        let status = content_host_attempt_status(port, "dl-0", "inst-A").await;
        assert_eq!(status, WsStatus::NOT_FOUND);
    }

    /// Symmetric admission on content too: a keyless content-join is 401'd
    /// (before any gateway signaling).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn content_join_without_an_admitted_key_is_rejected() {
        let port = serve().await;
        match connect_content_join(port, "node-1", None).await {
            Err(WsError::Http(resp)) => assert_eq!(resp.status(), WsStatus::UNAUTHORIZED),
            other => panic!("expected 401, got {other:?}"),
        }
    }

    /// An unadmitted key is refused at the control dial itself (the pre-layer
    /// 401s), so the gateway never registers and a phone naming it is refused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unadmitted_control_dial_is_rejected() {
        let port = serve().await;
        match connect_control(port, Some("bogus")).await {
            Err(WsError::Http(resp)) => assert_eq!(resp.status(), WsStatus::UNAUTHORIZED),
            other => panic!("expected 401, got {other:?}"),
        }
        // No gateway registered for node-x, so a phone naming it is refused.
        match connect_content_join(port, "node-x", Some("inst-A")).await {
            Err(WsError::Http(resp)) => assert_eq!(resp.status(), WsStatus::SERVICE_UNAVAILABLE),
            other => panic!("expected 503, got {other:?}"),
        }
    }

    /// Revoking an admitted gateway's remote_api_key (an admission reload that
    /// dropped it) kicks its already-live control connection, not just future
    /// dials.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn revoking_a_key_drops_its_live_control_connection() {
        use std::collections::HashSet;

        let admission = Arc::new(remote_host_admission::InMemoryAdmission::with_keys([
            "inst-A",
        ]));
        let conns = Arc::new(ConnectionRegistry::new());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = build_router(
            RelayServices {
                admission,
                conns: conns.clone(),
                bandwidth: Arc::new(BandwidthRegistry::new()),
                traffic: Arc::new(TrafficRegistry::new()),
                ip_traffic: Arc::new(IpTrafficRegistry::new()),
            },
            IpLimitConfig::socket_peer(),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut control = connect_control(port, Some("inst-A")).await.unwrap();
        let hello = serde_json::json!({ "relay_node_id": "node-1" });
        control
            .send(Message::Binary(serde_json::to_vec(&hello).unwrap()))
            .await
            .unwrap();

        // Registration is async (the server reads the hello, then registers), so
        // poll the kick until it finds the live connection and drops it.
        let revoked = HashSet::from(["inst-A".to_string()]);
        let mut kicked = 0;
        for _ in 0..40 {
            kicked = conns.kick(&revoked);
            if kicked > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(kicked, 1, "the live control connection was kicked");

        // The server closed it: the client sees a Close frame or EOF.
        let closed = loop {
            match control.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break true,
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                Some(Ok(other)) => panic!("unexpected frame after revoke: {other:?}"),
            }
        };
        assert!(
            closed,
            "control connection closed after its key was revoked"
        );
    }

    async fn recv_bin(ws: &mut WebSocketStream<TcpStream>) -> Vec<u8> {
        loop {
            match ws.next().await.unwrap().unwrap() {
                Message::Binary(b) => return b.to_vec(),
                Message::Ping(_) | Message::Pong(_) => continue,
                other => panic!("unexpected frame: {other:?}"),
            }
        }
    }
}
