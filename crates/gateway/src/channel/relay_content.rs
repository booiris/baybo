//! The gateway's relay-**content** control side.
//!
//! A NAT'd gateway can't be dialed, so for post-pairing chat it holds a
//! persistent outbound **control connection** to C (`/control`). When a phone
//! arrives at the relay for this gateway's `relay_node_id`, C pushes
//! [`ControlSignal::OpenDataLeg`]; the gateway dials a data leg
//! (`/content/host/{relay_key}`) and runs the Noise content responder over it
//! (see [`super::device_content::run_content_over_relay`]).
//!
//! There is **no `relay` config block**: the manager is driven by the single
//! approved device row (one gateway = one app). It idles until a device is
//! paired, then dials the relay URL + admission key recorded on that row at
//! pairing ([`baybo_store::DeviceRow::relay_url`] / `remote_api_key`), re-dialing
//! with a fixed backoff after any drop. When the device is revoked the control
//! connection is torn down promptly, so the gateway stops advertising a route it
//! can no longer authenticate a content session for.

use std::time::Duration;

use baybo_agent::service::ShutdownSignal;
use baybo_store::DeviceStatus;
use rand::RngExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::blob_content::run_blob_over_relay;
use super::device_content::run_content_over_relay;
use super::state::{LegDedup, WsChannelState};
use remote_host_protocol::key_tag;
use remote_host_protocol::relay::{LegClass, REMOTE_API_KEY_HEADER};

use crate::relay::{
    ControlCloseFrame, ControlHello, ControlSignal, connect_control, control_error_detail,
    load_or_create_relay_node_id, ws_error_detail,
};

/// Mean backoff between control-connection (re)dials. The actual wait is
/// jittered around this (see [`reconnect_delay`]).
const RECONNECT_BACKOFF: Duration = Duration::from_secs(5);

/// Multiplicative jitter applied to [`RECONNECT_BACKOFF`]: the wait is the base
/// times a factor in this range. A symmetric ±50% (mean unchanged) is enough to
/// **decorrelate phase** across a fleet — one C fronts many gateways, so without
/// jitter a C restart drops every gateway's control connection at once and they
/// all redial in lockstep at `T+5s, T+10s, …`, a synchronized accept/handshake
/// spike. This is phase spreading only, not a change to the recovery cadence.
const RECONNECT_JITTER: std::ops::Range<f64> = 0.5..1.5;

/// One jittered control-redial wait (see [`RECONNECT_JITTER`]).
fn reconnect_delay() -> Duration {
    RECONNECT_BACKOFF.mul_f64(rand::rng().random_range(RECONNECT_JITTER))
}

/// Poll cadence for the approved device row — both while idle (waiting for a
/// pairing) and while a control connection is live (watching for a revoke).
/// Cheap: one tiny libsql read per tick against a ≤1-row table.
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// A control connection that stayed up at least this long before failing counts
/// as having been healthy: the next failure opens a new warn cycle instead of
/// being debounced as another back-to-back redial attempt.
const HEALTHY_CONNECTION_MIN: Duration = Duration::from_secs(30);

/// How one control connection ended, for the caller's disconnect log.
enum ControlEnd {
    /// The relay closed an established connection; the Close frame (when it
    /// sent one) carries the relay's stated reason.
    ClosedByRelay(Option<ControlCloseFrame>),
    /// Torn down locally — device revoked/superseded or gateway shutdown.
    TornDown,
}

/// How a disconnect should be logged, after [`note_connection_ended`] folds it
/// into the reconnect failure cycle.
enum DisconnectKind {
    /// The connection had been healthy (up ≥ [`HEALTHY_CONNECTION_MIN`]); the
    /// failure cycle is reset so the next connect logs at info.
    Healthy,
    /// The first failure of a fresh cycle.
    FirstFailure,
    /// A back-to-back redial within an ongoing failure cycle.
    RepeatFailure,
}

/// Fold an ended control connection into the reconnect failure-cycle counter and
/// report how to log the disconnect. A connection that stayed up at least
/// [`HEALTHY_CONNECTION_MIN`] resets the cycle (emitting a one-line recovery when
/// it followed failures) and does NOT advance it, so the reconnect after a healthy
/// drop is visible at info; a shorter-lived one advances the cycle so a
/// connect-then-die loop goes quiet after its first visible line. Both disconnect
/// arms route through here so the reset-vs-advance accounting lives in one place.
fn note_connection_ended(
    attempt_started: std::time::Instant,
    consecutive_failures: &mut u32,
    relay_url: &str,
    relay_node_id: &str,
) -> DisconnectKind {
    if attempt_started.elapsed() >= HEALTHY_CONNECTION_MIN {
        if *consecutive_failures > 0 {
            tracing::info!(
                relay = %relay_url,
                relay_node_id = %relay_node_id,
                "relay-content: control connection recovered"
            );
        }
        *consecutive_failures = 0;
        return DisconnectKind::Healthy;
    }
    let first = *consecutive_failures == 0;
    *consecutive_failures = consecutive_failures.saturating_add(1);
    if first {
        DisconnectKind::FirstFailure
    } else {
        DisconnectKind::RepeatFailure
    }
}

/// The relay endpoint + admission key the gateway dials, resolved from the single
/// approved device row.
struct RelaySettings {
    relay_url: String,
    remote_api_key: String,
}

/// The outcome of resolving the approved device row. The three states are
/// distinct on purpose: a transient store read failure ([`Unavailable`]) must not
/// be mistaken for an authoritative "no usable device" ([`Absent`]), or a single
/// DB hiccup on a poll tick would tear down a live, healthy control connection
/// (and its in-flight content legs) instead of being retried.
///
/// [`Unavailable`]: RelayResolution::Unavailable
/// [`Absent`]: RelayResolution::Absent
enum RelayResolution {
    /// An approved device with recorded relay settings — dial / keep the link.
    Ready(RelaySettings),
    /// Authoritatively no usable device: none paired, revoked, or the row predates
    /// the relay fields (re-pair to populate).
    Absent,
    /// The device store read failed transiently — the device's state is unknown,
    /// so keep whatever connection is already up and retry on the next tick.
    Unavailable,
}

/// Resolve the relay settings from the approved device row (one gateway = one
/// app). `no_relay_diagnosed` remembers which device the missing-fields condition
/// was already reported for, so the permanent un-routability is surfaced once per
/// device rather than every poll tick.
async fn approved_relay_settings(
    state: &WsChannelState,
    no_relay_diagnosed: &mut Option<String>,
) -> RelayResolution {
    let rows = match state.device_store.list(Some(DeviceStatus::Approved)).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "relay-content: device store read failed; keeping any live control connection and retrying"
            );
            *no_relay_diagnosed = None;
            return RelayResolution::Unavailable;
        }
    };
    let Some(row) = rows.into_iter().next() else {
        *no_relay_diagnosed = None;
        return RelayResolution::Absent;
    };
    if row.relay_url.is_empty() || row.remote_api_key.is_empty() {
        // A paired device whose row predates the relay fields would otherwise idle
        // here forever with no diagnostic — distinct from the plain "no device
        // paired" case. Surface it so the silent un-routability is explainable.
        if no_relay_diagnosed.as_deref() != Some(row.device_id.as_str()) {
            tracing::info!(
                device = %row.device_id,
                "relay-content: approved device row lacks relay_url/remote_api_key; \
                 relay + push disabled until re-pair",
            );
            *no_relay_diagnosed = Some(row.device_id.clone());
        }
        return RelayResolution::Absent;
    }
    *no_relay_diagnosed = None;
    RelayResolution::Ready(RelaySettings {
        relay_url: row.relay_url,
        remote_api_key: row.remote_api_key,
    })
}

/// Spawn the relay-content control manager and return its [`JoinHandle`] so the
/// caller tracks it under the shared shutdown drain. Idles until a device is
/// paired; stops when `shutdown` fires, tearing down the live control connection
/// and any in-flight content data legs (no detached, un-drained child tasks).
pub(crate) fn spawn(state: WsChannelState, shutdown: ShutdownSignal) -> JoinHandle<()> {
    // The control connection dials `wss://` via tokio-tungstenite, which uses
    // rustls's process-default CryptoProvider. Our graph enables both aws-lc-rs
    // and ring, so install aws-lc-rs explicitly before the first dial or
    // connect_async panics. Idempotent — Err means one is already installed.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    tracing::info!("relay-content: control manager started");
    tokio::spawn(run(state, shutdown))
}

async fn run(state: WsChannelState, shutdown: ShutdownSignal) {
    // Loaded lazily inside the loop and cached once it succeeds: a transient vault
    // failure at boot then retries on the next tick instead of permanently
    // disabling relay control (and thus chat reachability) for the whole process.
    let mut node_id_cache: Option<String> = None;
    let mut no_relay_diagnosed: Option<String> = None;
    let mut was_paired = false;
    // Failures since the connection was last healthy: the first one of a cycle
    // warns, the rest of the redial cycle stays at debug.
    let mut consecutive_failures: u32 = 0;
    loop {
        if shutdown.is_shutdown() {
            break;
        }
        let relay_node_id = match &node_id_cache {
            Some(id) => id.clone(),
            None => match load_or_create_relay_node_id(&state.secret_vault).await {
                Ok(id) => {
                    node_id_cache = Some(id.clone());
                    id
                }
                Err(e) => {
                    tracing::warn!(error = %e, "relay-content: relay_node_id load failed; retrying");
                    tokio::select! {
                        _ = tokio::time::sleep(DEVICE_POLL_INTERVAL) => {}
                        _ = shutdown.wait() => break,
                    }
                    continue;
                }
            },
        };
        // Idle until a device is paired (and has recorded its relay settings),
        // but wake immediately on shutdown rather than after the full poll tick.
        // Absent and Unavailable both idle here — there's no live connection to
        // preserve between attempts, so a transient store error just retries.
        let RelayResolution::Ready(settings) =
            approved_relay_settings(&state, &mut no_relay_diagnosed).await
        else {
            was_paired = false;
            tokio::select! {
                _ = tokio::time::sleep(DEVICE_POLL_INTERVAL) => {}
                _ = shutdown.wait() => break,
            }
            continue;
        };
        let control_url = remote_host_protocol::relay::control_url(&settings.relay_url);
        if !was_paired {
            tracing::info!(
                relay = %settings.relay_url,
                "relay-content: device paired; holding control connection"
            );
            was_paired = true;
            consecutive_failures = 0;
        }
        let healthy_cycle = consecutive_failures == 0;
        let retry_in = reconnect_delay();
        let attempt_started = std::time::Instant::now();
        // `run_once` owns its child tasks (the control pump + per-signal data
        // legs) and drains them on return, so it is *not* wrapped in a cancelling
        // select! here — it handles `shutdown` internally and returns cleanly.
        match run_once(
            &state,
            &settings,
            &control_url,
            &relay_node_id,
            &shutdown,
            &mut no_relay_diagnosed,
            healthy_cycle,
        )
        .await
        {
            Ok(ControlEnd::ClosedByRelay(close)) => {
                let kind = note_connection_ended(
                    attempt_started,
                    &mut consecutive_failures,
                    &settings.relay_url,
                    &relay_node_id,
                );
                let close_code = close.as_ref().map(|f| f.code);
                let close_reason = close
                    .as_ref()
                    .map(|f| f.reason.as_str())
                    .unwrap_or_default();
                // A clean close of a healthy link is routine (info); a
                // connect-then-die loop warns on its first line then goes quiet.
                match kind {
                    DisconnectKind::Healthy => tracing::info!(
                        relay = %settings.relay_url,
                        relay_node_id = %relay_node_id,
                        close_code = ?close_code,
                        close_reason = %close_reason,
                        "relay-content: control connection closed by relay; redialing"
                    ),
                    DisconnectKind::FirstFailure => tracing::warn!(
                        relay = %settings.relay_url,
                        relay_node_id = %relay_node_id,
                        close_code = ?close_code,
                        close_reason = %close_reason,
                        "relay-content: control connection closed by relay; redialing"
                    ),
                    DisconnectKind::RepeatFailure => tracing::debug!(
                        relay = %settings.relay_url,
                        relay_node_id = %relay_node_id,
                        close_code = ?close_code,
                        close_reason = %close_reason,
                        "relay-content: control connection closed by relay; redialing"
                    ),
                }
            }
            Ok(ControlEnd::TornDown) => {
                consecutive_failures = 0;
            }
            Err(e) => {
                let kind = note_connection_ended(
                    attempt_started,
                    &mut consecutive_failures,
                    &settings.relay_url,
                    &relay_node_id,
                );
                // An unexpected drop (error) is worth a warn even after a healthy
                // run — more notable than a clean close — but the healthy reset in
                // note_connection_ended still makes the reconnect log at info.
                match kind {
                    DisconnectKind::RepeatFailure => tracing::debug!(
                        relay = %settings.relay_url,
                        key_tag = %key_tag(&settings.remote_api_key),
                        relay_node_id = %relay_node_id,
                        error = %e,
                        retry_in = ?retry_in,
                        "relay-content: control connection failed; redialing"
                    ),
                    DisconnectKind::Healthy | DisconnectKind::FirstFailure => tracing::warn!(
                        relay = %settings.relay_url,
                        key_tag = %key_tag(&settings.remote_api_key),
                        relay_node_id = %relay_node_id,
                        error = %e,
                        retry_in = ?retry_in,
                        "relay-content: control connection failed; redialing"
                    ),
                }
            }
        }
        if shutdown.is_shutdown() {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(retry_in) => {}
            _ = shutdown.wait() => break,
        }
    }
    tracing::info!("relay-content: control manager stopped");
}

/// Hold one control connection until it closes (or the device is revoked, or the
/// gateway shuts down), opening a content data leg for each `OpenDataLeg` signal C
/// pushes. Polls the device row alongside so a revoke tears the connection down
/// rather than letting it linger.
///
/// Owns every child task it spawns: the control pump (a [`JoinHandle`], aborted
/// on revoke/shutdown) and the per-signal data legs (a [`tokio::task::JoinSet`],
/// drained on return). Nothing is left detached, so the manager's drain on
/// shutdown actually reclaims this connection's work.
async fn run_once(
    state: &WsChannelState,
    settings: &RelaySettings,
    control_url: &str,
    relay_node_id: &str,
    shutdown: &ShutdownSignal,
    no_relay_diagnosed: &mut Option<String>,
    healthy_cycle: bool,
) -> Result<ControlEnd, String> {
    let (tx, mut rx) = mpsc::channel::<ControlSignal>(32);
    let pump = tokio::spawn({
        let hello = ControlHello {
            relay_node_id: relay_node_id.to_owned(),
        };
        let control_url = control_url.to_owned();
        let remote_api_key = settings.remote_api_key.clone();
        async move { connect_control(&control_url, &remote_api_key, &hello, tx, healthy_cycle).await }
    });

    // In-flight content data legs. Tracked (not detached) so they're aborted when
    // this connection ends, and reaped as they finish so the set can't grow
    // without bound over a long-lived control connection.
    let mut legs: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    let mut poll = tokio::time::interval(DEVICE_POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    poll.tick().await; // the first tick fires immediately; skip it

    // `true` once we abort the pump ourselves (device revoked or gateway
    // shutdown), so its cancellation JoinError isn't surfaced as a connection
    // error.
    let mut pump_aborted = false;
    loop {
        tokio::select! {
            signal = rx.recv() => match signal {
                Some(ControlSignal::OpenDataLeg { relay_key, class }) => {
                    tracing::info!(
                        class = ?class,
                        relay_key = %key_tag(&relay_key),
                        "relay-content: OpenDataLeg received; dialing content host leg"
                    );
                    let state = state.clone();
                    let relay_url = settings.relay_url.clone();
                    let remote_api_key = settings.remote_api_key.clone();
                    // Hand the leg its own AbortHandle over a oneshot so the content
                    // session can register it in the device-dedup registry once Noise
                    // resolves the device_id — and a newer leg can abort this one.
                    let (ah_tx, ah_rx) = tokio::sync::oneshot::channel();
                    let abort = legs.spawn(async move {
                        open_data_leg(
                            &state,
                            &relay_url,
                            &remote_api_key,
                            &relay_key,
                            class,
                            ah_rx,
                        )
                        .await;
                    });
                    let _ = ah_tx.send(abort);
                }
                // The control connection closed (the pump dropped `tx`).
                None => break,
            },
            _ = poll.tick() => {
                // Tear down only on an authoritative Absent (revoked/superseded);
                // a transient Unavailable keeps the live connection (its warn is
                // already logged) and retries on the next tick.
                if let RelayResolution::Absent =
                    approved_relay_settings(state, no_relay_diagnosed).await
                {
                    tracing::info!(
                        relay = %settings.relay_url,
                        "relay-content: approved relay device gone (revoked or superseded); \
                         tearing down control connection"
                    );
                    pump.abort();
                    pump_aborted = true;
                    break;
                }
            }
            // Reap a finished data leg (disabled while none are in flight).
            Some(_) = legs.join_next(), if !legs.is_empty() => {}
            // The gateway is shutting down: stop accepting signals, abort the
            // pump, and fall through to drain the in-flight legs below.
            _ = shutdown.wait() => {
                pump.abort();
                pump_aborted = true;
                break;
            }
        }
    }

    // Abort and await any still-running data legs. A relayed content session is
    // best-effort — it just drops and the phone reconnects — so a hard abort is
    // fine; awaiting it keeps the drain bounded and leaves nothing detached.
    legs.shutdown().await;

    let outcome = pump.await;
    if pump_aborted {
        // Even if the pump ended on its own right as we tore it down, this was a
        // local teardown from the caller's perspective.
        return match outcome {
            Ok(_) => Ok(ControlEnd::TornDown),
            Err(e) if e.is_cancelled() => Ok(ControlEnd::TornDown),
            Err(e) => Err(format!("control task panicked: {e}")),
        };
    }
    match outcome {
        Ok(Ok(close)) => Ok(ControlEnd::ClosedByRelay(close)),
        Ok(Err(e)) => Err(control_error_detail(&e)),
        Err(e) => Err(format!("control task panicked: {e}")),
    }
}

/// Dial a content data leg for `relay_key` and run the responder for its `class`:
/// the Noise chat content session ([`LegClass::Chat`]) or the blob sub-protocol
/// ([`LegClass::Blob`]). `ah_rx` delivers this task's own
/// [`AbortHandle`](tokio::task::AbortHandle) (sent by the spawner), which the
/// session registers in the matching device-dedup registry once it resolves the
/// `device_id` — chat and blob use *separate* registries so a blob leg never
/// aborts the chat leg.
async fn open_data_leg(
    state: &WsChannelState,
    relay_url: &str,
    remote_api_key: &str,
    relay_key: &str,
    class: LegClass,
    ah_rx: tokio::sync::oneshot::Receiver<tokio::task::AbortHandle>,
) {
    let started = std::time::Instant::now();
    let url = remote_host_protocol::relay::content_host_url(relay_url, relay_key);
    let mut req = match url.into_client_request() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "relay-content: bad data-leg url");
            return;
        }
    };
    match remote_api_key.parse() {
        Ok(v) => {
            req.headers_mut().insert(REMOTE_API_KEY_HEADER, v);
        }
        Err(e) => {
            tracing::warn!(error = %e, "relay-content: bad remote_api_key header");
            return;
        }
    }
    let ws = match connect_async(req).await {
        Ok((ws, _)) => ws,
        Err(e) => {
            tracing::warn!(
                class = ?class,
                relay_key = %key_tag(relay_key),
                relay = %relay_url,
                error = %ws_error_detail(&e),
                "relay-content: data-leg connect failed; phone waiting at relay will not be served"
            );
            return;
        }
    };
    tracing::info!(
        class = ?class,
        relay_key = %key_tag(relay_key),
        "relay-content: data leg established"
    );
    match class {
        // Blob legs run **concurrently** — one per transfer — so they are NOT
        // deduped: a second blob transfer for the same device must not abort the
        // first. Concurrency is bounded by the relay's per-key connection cap.
        // (The leg's own AbortHandle oneshot goes unused; drop it.)
        LegClass::Blob => {
            drop(ah_rx);
            run_blob_over_relay(ws, state).await;
        }
        // Chat dedups to one live leg per device: learn our own AbortHandle (sent
        // right after spawn) so a fresh leg aborts a stale predecessor; if it never
        // arrives, run without dedup.
        LegClass::Chat => {
            let dedup = ah_rx.await.ok().map(|abort| LegDedup {
                registry: state.device_leg_registry.clone(),
                abort,
            });
            run_content_over_relay(ws, state, dedup).await;
        }
    }
    tracing::info!(
        class = ?class,
        relay_key = %key_tag(relay_key),
        duration = ?started.elapsed(),
        "relay-content: data leg ended"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::build_test_deps;

    /// While idle (no device paired), the manager must return promptly when
    /// shutdown fires — not run until the next poll tick, and certainly not leak
    /// as a detached task. Proves the idle-loop honours the shared signal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manager_stops_promptly_on_shutdown_while_idle() {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        let state = WsChannelState::from_deps(&tg.deps);
        let shutdown = ShutdownSignal::new();
        let handle = spawn(state, shutdown.clone());

        // No approved device row → the manager idles in its poll loop. Shutdown
        // must unblock it well inside the DEVICE_POLL_INTERVAL.
        shutdown.trigger();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("manager returns promptly on shutdown")
            .expect("manager task did not panic");
    }

    /// The jittered redial wait always lands in [0.5×, 1.5×] of the base, so phase
    /// is spread without ever collapsing to ~0 (a hot redial) or drifting far past
    /// the intended cadence. Sampled over many draws to exercise the range.
    #[test]
    fn reconnect_delay_stays_within_the_jitter_band() {
        let lo = RECONNECT_BACKOFF.mul_f64(RECONNECT_JITTER.start);
        let hi = RECONNECT_BACKOFF.mul_f64(RECONNECT_JITTER.end);
        let mut saw_below_base = false;
        let mut saw_above_base = false;
        for _ in 0..1_000 {
            let d = reconnect_delay();
            assert!(d >= lo && d < hi, "delay {d:?} outside [{lo:?}, {hi:?})");
            saw_below_base |= d < RECONNECT_BACKOFF;
            saw_above_base |= d > RECONNECT_BACKOFF;
        }
        // The jitter is two-sided (not just a one-directional shave).
        assert!(
            saw_below_base && saw_above_base,
            "jitter should spread both under and over the base"
        );
    }
}
