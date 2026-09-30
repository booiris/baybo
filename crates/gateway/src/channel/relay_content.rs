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
//! with a fixed backoff after any drop. When the device is revoked or re-paired
//! against different relay settings, the old control connection is torn down
//! promptly so the gateway stops advertising a stale route.
//!
//! Each distinct approved binding is one **binding scope**: entered when the
//! row resolves to `Ready(settings)`, left on a Reconfigure, a TearDown or
//! shutdown. The scope owns the binding's [`CarrierRuntime`], started before
//! the first control connection and stopped when the scope ends. Control
//! redials happen inside the scope, so a control flap never restarts the
//! runtime or drops its carrier sessions.

use std::time::Duration;

use baybo_agent::service::ShutdownSignal;
use baybo_store::DeviceStatus;
use rand::RngExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::api_tunnel::run_api_tunnel_over_relay;
use super::carrier::runtime::{BindingDevice, CarrierProcess, CarrierRuntime};
use super::device_content::run_content_over_relay;
use super::state::{LegDedup, WsChannelState};
use remote_host_protocol::REMOTE_API_KEY_HEADER;
use remote_host_protocol::key_tag;
use remote_host_protocol::relay::{ControlReport, LegClass};

use crate::config::RuntimeCarrierConfig;
use crate::relay::{
    ControlChannels, ControlCloseFrame, ControlHello, ControlSignal, connect_control,
    control_error_detail, load_or_create_relay_node_id, ws_error_detail,
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

/// Poll cadence for the approved device row — both while idle (waiting for a
/// pairing) and while a control connection is live (watching for a revoke).
/// Cheap: one tiny sqlite read per tick against a ≤1-row table.
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Signals queued from one control connection to the manager, and reports
/// queued from the manager back to it.
const CONTROL_QUEUE_CAPACITY: usize = 32;

/// The manager's waits: [`ControlTiming::PRODUCTION`] outside tests, which
/// shrink them to milliseconds.
#[derive(Debug, Clone, Copy)]
pub(super) struct ControlTiming {
    reconnect_backoff: Duration,
    device_poll: Duration,
}

impl ControlTiming {
    const PRODUCTION: Self = Self {
        reconnect_backoff: RECONNECT_BACKOFF,
        device_poll: DEVICE_POLL_INTERVAL,
    };

    /// Millisecond waits, so a test notices a revoke or redials without
    /// sitting out a production poll.
    #[cfg(test)]
    pub(super) const FAST: Self = Self {
        reconnect_backoff: Duration::from_millis(40),
        device_poll: Duration::from_millis(40),
    };

    /// One jittered control-redial wait (see [`RECONNECT_JITTER`]).
    fn reconnect_delay(self) -> Duration {
        self.reconnect_backoff
            .mul_f64(rand::rng().random_range(RECONNECT_JITTER))
    }
}

/// A control connection that stayed up at least this long before failing counts
/// as having been healthy: the next failure opens a new warn cycle instead of
/// being debounced as another back-to-back redial attempt.
const HEALTHY_CONNECTION_MIN: Duration = Duration::from_secs(30);

/// How one control connection ended, for the caller's disconnect log.
enum ControlEnd {
    /// The relay closed an established connection; the Close frame (when it
    /// sent one) carries the relay's stated reason.
    ClosedByRelay(Option<ControlCloseFrame>),
    /// Torn down locally because the binding scope ends: the device row was
    /// re-paired with different settings, revoked or superseded, or the gateway
    /// is shutting down.
    ScopeEnded,
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

/// The binding resolved from the single approved device row: the relay endpoint
/// and admission key the gateway dials, and the device's identity and
/// credentials. Equality covers all of them, so a re-pair or credential change
/// is a Reconfigure, just like a relay URL change.
#[derive(Clone, PartialEq, Eq)]
struct RelaySettings {
    device_id: String,
    device_pubkey: Vec<u8>,
    auth_token_sha256: String,
    approved_at: Option<i64>,
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

#[derive(Debug, PartialEq, Eq)]
enum ControlBindingState {
    Keep,
    Reconfigure,
    TearDown,
}

fn control_binding_state(
    resolution: &RelayResolution,
    active: &RelaySettings,
) -> ControlBindingState {
    match resolution {
        RelayResolution::Ready(current) if current != active => ControlBindingState::Reconfigure,
        RelayResolution::Ready(_) | RelayResolution::Unavailable => ControlBindingState::Keep,
        RelayResolution::Absent => ControlBindingState::TearDown,
    }
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
        device_id: row.device_id,
        device_pubkey: row.device_pubkey,
        auth_token_sha256: row.auth_token_sha256,
        approved_at: row.approved_at,
        relay_url: row.relay_url,
        remote_api_key: row.remote_api_key,
    })
}

/// Spawn the relay-content control manager and return its [`JoinHandle`] so the
/// caller tracks it under the shared shutdown drain. Idles until a device is
/// paired; stops when `shutdown` fires, tearing down the live control connection,
/// any in-flight content data legs and the binding's carrier runtime (no
/// detached, un-drained child tasks).
pub(crate) fn spawn(
    state: WsChannelState,
    carrier: RuntimeCarrierConfig,
    shutdown: ShutdownSignal,
) -> JoinHandle<()> {
    // The control connection dials `wss://` via tokio-tungstenite, which uses
    // rustls's process-default CryptoProvider. Our graph enables both aws-lc-rs
    // and ring, so install aws-lc-rs explicitly before the first dial or
    // connect_async panics. Idempotent — Err means one is already installed.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    tracing::debug!("relay-content: control manager started");
    tokio::spawn(run(state, carrier, shutdown, ControlTiming::PRODUCTION))
}

pub(super) async fn run(
    state: WsChannelState,
    carrier: RuntimeCarrierConfig,
    shutdown: ShutdownSignal,
    timing: ControlTiming,
) {
    // Loaded lazily inside the loop and cached once it succeeds: a transient vault
    // failure at boot then retries on the next tick instead of permanently
    // disabling relay control (and thus chat reachability) for the whole process.
    let mut node_id_cache: Option<String> = None;
    let mut no_relay_diagnosed: Option<String> = None;
    // A's QUIC certificate and offer replay cache are per process, shared by
    // every binding scope's runtime.
    let mut process = CarrierProcess::new();
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
                        _ = tokio::time::sleep(timing.device_poll) => {}
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
            tokio::select! {
                _ = tokio::time::sleep(timing.device_poll) => {}
                _ = shutdown.wait() => break,
            }
            continue;
        };
        let started = CarrierRuntime::start(
            &carrier,
            &mut process,
            &state,
            &relay_node_id,
            BindingDevice {
                device_id: &settings.device_id,
                device_pubkey: &settings.device_pubkey,
            },
        )
        .await;
        let mut runtime = match started {
            Ok(runtime) => runtime,
            // Every relay leg's handshake reads the same static key, so
            // control could serve nothing until it is readable again. Waiting
            // here keeps the capability in the scope's first hello.
            Err(error) => {
                tracing::warn!(
                    device = %super::short_hash(&settings.device_id),
                    %error,
                    "relay-content: the gateway's static key is unreadable; retrying before holding control"
                );
                tokio::select! {
                    _ = tokio::time::sleep(timing.device_poll) => {}
                    _ = shutdown.wait() => break,
                }
                continue;
            }
        };
        tracing::info!(
            relay = %settings.relay_url,
            device = %super::short_hash(&settings.device_id),
            direct = ?runtime.capability(),
            "relay-content: approved device present; holding control connection"
        );
        let mut scope = BindingScope {
            state: &state,
            settings: &settings,
            relay_node_id: &relay_node_id,
            control_url: remote_host_protocol::relay::control_url(&settings.relay_url),
            shutdown: &shutdown,
            timing,
            carrier: &mut runtime,
        };
        run_binding(&mut scope, &mut no_relay_diagnosed).await;
        runtime.stop().await;
        tracing::debug!(
            device = %super::short_hash(&settings.device_id),
            "relay-content: binding scope ended; carrier runtime stopped"
        );
    }
    tracing::debug!("relay-content: control manager stopped");
}

/// What every control connection of one binding scope shares: the settings the
/// scope was entered for and the carrier runtime it owns.
struct BindingScope<'a> {
    state: &'a WsChannelState,
    settings: &'a RelaySettings,
    relay_node_id: &'a str,
    control_url: String,
    shutdown: &'a ShutdownSignal,
    timing: ControlTiming,
    carrier: &'a mut CarrierRuntime,
}

/// The control redial loop of one binding scope. Returns when the binding ends:
/// the device row reconfigures or tears it down (noticed on a live
/// connection's poll or when a redial re-resolves the row), or the gateway
/// shuts down. A connection that closes or fails is redialed inside the scope.
async fn run_binding(scope: &mut BindingScope<'_>, no_relay_diagnosed: &mut Option<String>) {
    let settings = scope.settings;
    // Failures since the connection was last healthy: the first one of a cycle
    // warns, the rest of the redial cycle stays at debug.
    let mut consecutive_failures: u32 = 0;
    loop {
        let healthy_cycle = consecutive_failures == 0;
        let retry_in = scope.timing.reconnect_delay();
        let attempt_started = std::time::Instant::now();
        // `run_once` owns its child tasks (the control pump + per-signal data
        // legs) and drains them on return, so it is *not* wrapped in a cancelling
        // select! here — it handles `shutdown` internally and returns cleanly.
        match run_once(scope, no_relay_diagnosed, healthy_cycle).await {
            Ok(ControlEnd::ClosedByRelay(close)) => {
                let kind = note_connection_ended(
                    attempt_started,
                    &mut consecutive_failures,
                    &settings.relay_url,
                    scope.relay_node_id,
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
                        relay_node_id = %scope.relay_node_id,
                        close_code = ?close_code,
                        close_reason = %close_reason,
                        "relay-content: control connection closed by relay; redialing"
                    ),
                    DisconnectKind::FirstFailure => tracing::warn!(
                        relay = %settings.relay_url,
                        relay_node_id = %scope.relay_node_id,
                        close_code = ?close_code,
                        close_reason = %close_reason,
                        "relay-content: control connection closed by relay; redialing"
                    ),
                    DisconnectKind::RepeatFailure => tracing::debug!(
                        relay = %settings.relay_url,
                        relay_node_id = %scope.relay_node_id,
                        close_code = ?close_code,
                        close_reason = %close_reason,
                        "relay-content: control connection closed by relay; redialing"
                    ),
                }
            }
            Ok(ControlEnd::ScopeEnded) => return,
            Err(e) => {
                let kind = note_connection_ended(
                    attempt_started,
                    &mut consecutive_failures,
                    &settings.relay_url,
                    scope.relay_node_id,
                );
                // An unexpected drop (error) is worth a warn even after a healthy
                // run — more notable than a clean close — but the healthy reset in
                // note_connection_ended still makes the reconnect log at info.
                match kind {
                    DisconnectKind::RepeatFailure => tracing::debug!(
                        relay = %settings.relay_url,
                        key_tag = %key_tag(&settings.remote_api_key),
                        relay_node_id = %scope.relay_node_id,
                        error = %e,
                        retry_in = ?retry_in,
                        "relay-content: control connection failed; redialing"
                    ),
                    DisconnectKind::Healthy | DisconnectKind::FirstFailure => tracing::warn!(
                        relay = %settings.relay_url,
                        key_tag = %key_tag(&settings.remote_api_key),
                        relay_node_id = %scope.relay_node_id,
                        error = %e,
                        retry_in = ?retry_in,
                        "relay-content: control connection failed; redialing"
                    ),
                }
            }
        }
        if scope.shutdown.is_shutdown() {
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(retry_in) => {}
            _ = scope.shutdown.wait() => return,
        }
        // The redial re-resolves the row: a revoke or re-pair that landed while
        // control was down ends the scope here instead of on the next dial.
        let resolution = approved_relay_settings(scope.state, no_relay_diagnosed).await;
        if binding_ends(&resolution, settings) {
            return;
        }
    }
}

/// Whether a fresh resolution ends the binding scope entered for `active`
/// (a Reconfigure or a TearDown), logging why when it does.
fn binding_ends(resolution: &RelayResolution, active: &RelaySettings) -> bool {
    match control_binding_state(resolution, active) {
        ControlBindingState::Keep => false,
        ControlBindingState::Reconfigure => {
            if let RelayResolution::Ready(current) = resolution {
                tracing::info!(
                    old_relay = %active.relay_url,
                    new_relay = %current.relay_url,
                    old_key_tag = %key_tag(&active.remote_api_key),
                    new_key_tag = %key_tag(&current.remote_api_key),
                    old_device = %super::short_hash(&active.device_id),
                    new_device = %super::short_hash(&current.device_id),
                    "relay-content: approved relay binding changed; moving control connection"
                );
            }
            true
        }
        ControlBindingState::TearDown => {
            tracing::info!(
                relay = %active.relay_url,
                device = %super::short_hash(&active.device_id),
                "relay-content: approved relay device gone (revoked or superseded); \
                 tearing down control connection"
            );
            true
        }
    }
}

/// Hold one control connection until it closes (or the device is revoked,
/// re-paired, or the gateway shuts down), opening a content data leg for each
/// `OpenDataLeg` signal C pushes and answering each `DirectOffer` through the
/// scope's carrier runtime. Polls the device row alongside so a revoke or
/// binding change tears the stale connection down rather than letting it
/// linger.
///
/// Owns every child task it spawns: the control pump (a [`JoinHandle`], aborted
/// on revoke/shutdown) and the per-signal data legs (a [`tokio::task::JoinSet`],
/// drained on return). Nothing is left detached, so the manager's drain on
/// shutdown actually reclaims this connection's work.
async fn run_once(
    scope: &mut BindingScope<'_>,
    no_relay_diagnosed: &mut Option<String>,
    healthy_cycle: bool,
) -> Result<ControlEnd, String> {
    let settings = scope.settings;
    let (signals, mut rx) = mpsc::channel::<ControlSignal>(CONTROL_QUEUE_CAPACITY);
    let (report_tx, reports) = mpsc::channel::<ControlReport>(CONTROL_QUEUE_CAPACITY);
    let pump = tokio::spawn({
        let hello = ControlHello {
            relay_node_id: scope.relay_node_id.to_owned(),
            direct: scope.carrier.capability(),
        };
        let control_url = scope.control_url.clone();
        let remote_api_key = settings.remote_api_key.clone();
        let channels = ControlChannels { signals, reports };
        async move {
            connect_control(
                &control_url,
                &remote_api_key,
                &hello,
                channels,
                healthy_cycle,
            )
            .await
        }
    });

    // In-flight content data legs. Tracked (not detached) so they're aborted when
    // this connection ends, and reaped as they finish so the set can't grow
    // without bound over a long-lived control connection.
    let mut legs: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    let mut poll = tokio::time::interval(scope.timing.device_poll);
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
                    tracing::debug!(
                        class = ?class,
                        relay_key = %key_tag(&relay_key),
                        "relay-content: OpenDataLeg received; dialing content host leg"
                    );
                    let state = scope.state.clone();
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
                Some(ControlSignal::DirectOffer { punch_id, offer, register }) => {
                    let report = scope.carrier.handle_offer(punch_id, offer, register);
                    // Never waits: a report that finds this connection gone or
                    // backed up is dropped, and C answers the phone `504`.
                    if let Err(e) = report_tx.try_send(report) {
                        tracing::debug!(
                            punch = %punch_id.tag(),
                            error = %e,
                            "relay-content: direct report dropped"
                        );
                    }
                }
                // The control connection closed (the pump dropped `signals`).
                None => break,
            },
            _ = poll.tick() => {
                let resolution = approved_relay_settings(scope.state, no_relay_diagnosed).await;
                if binding_ends(&resolution, settings) {
                    pump.abort();
                    pump_aborted = true;
                    break;
                }
            }
            // Reap a finished data leg (disabled while none are in flight).
            Some(_) = legs.join_next(), if !legs.is_empty() => {}
            // The gateway is shutting down: stop accepting signals, abort the
            // pump, and fall through to drain the in-flight legs below.
            _ = scope.shutdown.wait() => {
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
        match outcome {
            Ok(_) => {}
            Err(e) if e.is_cancelled() => {}
            Err(e) => return Err(format!("control task panicked: {e}")),
        }
        return Ok(ControlEnd::ScopeEnded);
    }
    match outcome {
        Ok(Ok(close)) => Ok(ControlEnd::ClosedByRelay(close)),
        Ok(Err(e)) => Err(control_error_detail(&e)),
        Err(e) => Err(format!("control task panicked: {e}")),
    }
}

/// Dial a content data leg for `relay_key` and run the responder for its `class`:
/// the Noise chat content session ([`LegClass::Chat`]) or the API tunnel
/// ([`LegClass::Api`] / [`LegClass::Blob`]). `ah_rx` delivers this task's own
/// [`AbortHandle`](tokio::task::AbortHandle) (sent by the spawner), which the
/// session registers in the matching device-dedup registry once it resolves the
/// `device_id`. Blob legs are not deduped; concurrent transfers are bounded by
/// the relay connection cap.
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
        // The error stringifies the request URL, which embeds the raw relay_key;
        // log the sanitized key_tag instead so the credential never reaches a log.
        Err(_) => {
            tracing::warn!(
                class = ?class,
                relay_key = %key_tag(relay_key),
                relay = %relay_url,
                "relay-content: bad data-leg url"
            );
            return;
        }
    };
    match remote_api_key.parse() {
        Ok(v) => {
            req.headers_mut().insert(REMOTE_API_KEY_HEADER, v);
        }
        Err(e) => {
            tracing::warn!(
                class = ?class,
                relay_key = %key_tag(relay_key),
                error = %e,
                "relay-content: bad remote_api_key header"
            );
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
        // Tunnel legs are never deduped: an `Api` leg may serve several requests
        // in sequence and a `Blob` leg carries one long transfer, but a device
        // legitimately runs more than one of either at a time. Concurrency is
        // bounded by the relay's per-key connection cap. (The leg's own
        // AbortHandle oneshot goes unused; drop it.) The class travels with the
        // leg because only `Api` may be reused — see `run_api_tunnel_over_relay`.
        LegClass::Api | LegClass::Blob => {
            drop(ah_rx);
            run_api_tunnel_over_relay(ws, class, state).await;
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
    use std::io::ErrorKind;
    use std::net::{SocketAddr, UdpSocket};

    use axum::Router;
    use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
    use axum::routing::get;
    use baybo_store::DeviceRow;
    use remote_host_protocol::relay::{
        DIRECT_PROTOCOL_VERSION, DirectCapability, PunchId, SealedCandidates,
    };

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use baybo_security::SecretVault;
    use baybo_store::secret::Result as SecretResult;
    use baybo_store::{SecretStore, StorageError, StoreIdentity};

    use super::*;
    use crate::config::FamilyBinds;
    use crate::device::NOISE_STATIC_VAULT_KEY;
    use crate::test_support::{TestGateway, build_test_deps};

    const STEP_TIMEOUT: Duration = Duration::from_secs(5);
    const DEVICE_ID: &str = "device-1";
    const REMOTE_API_KEY: &str = "inst-A";

    fn relay_settings(relay_url: &str, remote_api_key: &str) -> RelaySettings {
        RelaySettings {
            device_id: DEVICE_ID.to_owned(),
            device_pubkey: vec![7; 32],
            auth_token_sha256: "sha256:device".to_owned(),
            approved_at: Some(0),
            relay_url: relay_url.to_owned(),
            remote_api_key: remote_api_key.to_owned(),
        }
    }

    #[test]
    fn live_control_binding_reconfigures_when_relay_settings_change() {
        let active = relay_settings("wss://old.example", "old-key");

        assert_eq!(
            control_binding_state(&RelayResolution::Ready(active.clone()), &active),
            ControlBindingState::Keep
        );
        assert_eq!(
            control_binding_state(
                &RelayResolution::Ready(relay_settings("wss://new.example", "old-key")),
                &active,
            ),
            ControlBindingState::Reconfigure
        );
        assert_eq!(
            control_binding_state(
                &RelayResolution::Ready(relay_settings("wss://old.example", "new-key")),
                &active,
            ),
            ControlBindingState::Reconfigure
        );
        assert_eq!(
            control_binding_state(&RelayResolution::Unavailable, &active),
            ControlBindingState::Keep
        );
        assert_eq!(
            control_binding_state(&RelayResolution::Absent, &active),
            ControlBindingState::TearDown
        );
    }

    /// A re-pair or credential change is a Reconfigure even when the relay URL
    /// and admission key stay the same.
    #[test]
    fn a_device_identity_or_credential_change_reconfigures_the_binding() {
        let active = relay_settings("wss://relay.example", "key");
        let changes: [fn(&mut RelaySettings); 4] = [
            |settings| settings.device_id.push('2'),
            |settings| settings.device_pubkey[0] ^= 1,
            |settings| settings.auth_token_sha256.push('2'),
            |settings| settings.approved_at = Some(1),
        ];
        for change in changes {
            let mut current = active.clone();
            change(&mut current);
            assert_eq!(
                control_binding_state(&RelayResolution::Ready(current), &active),
                ControlBindingState::Reconfigure
            );
        }
    }

    /// While idle (no device paired), the manager must return promptly when
    /// shutdown fires — not run until the next poll tick, and certainly not leak
    /// as a detached task. Proves the idle-loop honours the shared signal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manager_stops_promptly_on_shutdown_while_idle() {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        let state = WsChannelState::from_deps(&tg.deps);
        let shutdown = ShutdownSignal::new();
        let handle = spawn(state, no_carriers(), shutdown.clone());

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
            let d = ControlTiming::PRODUCTION.reconnect_delay();
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

    fn no_carriers() -> RuntimeCarrierConfig {
        RuntimeCarrierConfig { udp: None }
    }

    /// A stand-in for C's `/control` route that hands each accepted connection
    /// to the test.
    struct MockC {
        port: u16,
        connections: mpsc::Receiver<WebSocket>,
        server: JoinHandle<()>,
    }

    impl MockC {
        async fn start() -> Self {
            let (accepted, connections) = mpsc::channel::<WebSocket>(4);
            let app = Router::new().route(
                "/control",
                get(move |ws: WebSocketUpgrade| {
                    let accepted = accepted.clone();
                    async move {
                        ws.on_upgrade(move |socket| async move {
                            let _ = accepted.send(socket).await;
                        })
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let _ = axum::serve(listener, app.into_make_service()).await;
            });
            Self {
                port,
                connections,
                server,
            }
        }

        fn relay_url(&self) -> String {
            format!("ws://127.0.0.1:{}", self.port)
        }

        async fn accept(&mut self) -> WebSocket {
            tokio::time::timeout(STEP_TIMEOUT, self.connections.recv())
                .await
                .expect("the gateway dials control")
                .expect("mock C is serving")
        }
    }

    impl Drop for MockC {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    /// The next binary frame from the gateway, or `None` once the connection
    /// has ended.
    async fn next_binary(socket: &mut WebSocket) -> Option<Vec<u8>> {
        loop {
            let message = tokio::time::timeout(STEP_TIMEOUT, socket.recv())
                .await
                .expect("control frame timed out");
            match message {
                Some(Ok(WsMessage::Binary(bytes))) => return Some(bytes.to_vec()),
                Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_))) => continue,
                Some(Ok(WsMessage::Close(_)) | Err(_)) | None => return None,
                Some(Ok(other)) => panic!("unexpected control frame: {other:?}"),
            }
        }
    }

    async fn read_hello(socket: &mut WebSocket) -> ControlHello {
        let bytes = next_binary(socket).await.expect("hello");
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn next_report(socket: &mut WebSocket) -> ControlReport {
        let bytes = next_binary(socket).await.expect("report");
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn send_offer(socket: &mut WebSocket, punch_id: PunchId) {
        let offer = ControlSignal::DirectOffer {
            punch_id,
            offer: SealedCandidates {
                n: "bm9uY2U=".to_owned(),
                enc: "Y2lwaGVydGV4dA==".to_owned(),
            },
            register: None,
        };
        socket
            .send(WsMessage::Binary(
                serde_json::to_vec(&offer).unwrap().into(),
            ))
            .await
            .unwrap();
    }

    fn device_row(relay_url: String) -> DeviceRow {
        DeviceRow {
            device_id: DEVICE_ID.to_owned(),
            device_pubkey: vec![7; 32],
            auth_token_sha256: baybo_store::device::hash_auth_token(
                "device-auth-token-fixed-0123456789abcdef",
            ),
            status: DeviceStatus::Approved,
            rendezvous_id: None,
            created_at: 0,
            approved_at: Some(0),
            last_seen_at: None,
            relay_url,
            push_url: "https://push.test".to_owned(),
            remote_api_key: REMOTE_API_KEY.to_owned(),
        }
    }

    /// A gateway with one approved device bound to `c`, its manager running
    /// on the fast timing.
    async fn start_manager(
        c: &MockC,
        carrier: RuntimeCarrierConfig,
    ) -> (TestGateway, ShutdownSignal, JoinHandle<()>) {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        tg.deps
            .stores
            .device
            .create(&device_row(c.relay_url()))
            .await
            .expect("seed the approved device");
        let shutdown = ShutdownSignal::new();
        let manager = tokio::spawn(run(
            WsChannelState::from_deps(&tg.deps),
            carrier,
            shutdown.clone(),
            ControlTiming::FAST,
        ));
        (tg, shutdown, manager)
    }

    async fn stop_manager(shutdown: ShutdownSignal, manager: JoinHandle<()>) {
        shutdown.trigger();
        tokio::time::timeout(STEP_TIMEOUT, manager)
            .await
            .expect("manager returns on shutdown")
            .expect("manager task did not panic");
    }

    fn port_is_held(address: SocketAddr) -> bool {
        match UdpSocket::bind(address) {
            Ok(_) => false,
            Err(error) if error.kind() == ErrorKind::AddrInUse => true,
            Err(error) => panic!("probe bind of {address}: {error}"),
        }
    }

    const UDP_CAPABILITY: Option<DirectCapability> = Some(DirectCapability {
        version: DIRECT_PROTOCOL_VERSION,
        udp: true,
    });

    /// The redial loop lives inside the scope: connections that the relay
    /// closes are redialed, each hello from the same runtime, and the scope
    /// ends only when the gateway shuts down.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_control_flap_stays_inside_the_binding_scope() {
        let mut c = MockC::start().await;
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        tg.deps
            .stores
            .device
            .create(&device_row(c.relay_url()))
            .await
            .expect("seed the approved device");
        let state = WsChannelState::from_deps(&tg.deps);
        let RelayResolution::Ready(settings) = approved_relay_settings(&state, &mut None).await
        else {
            panic!("the seeded device resolves to a binding");
        };
        let shutdown = ShutdownSignal::new();
        let scope_shutdown = shutdown.clone();
        let scope = tokio::spawn(async move {
            let carrier = RuntimeCarrierConfig {
                udp: Some(FamilyBinds {
                    ipv4: Some("127.0.0.1:0".parse().unwrap()),
                    ipv6: None,
                }),
            };
            let mut runtime = CarrierRuntime::start(
                &carrier,
                &mut CarrierProcess::new(),
                &state,
                "node-1",
                BindingDevice {
                    device_id: &settings.device_id,
                    device_pubkey: &settings.device_pubkey,
                },
            )
            .await
            .expect("the binding's candidate keys derive");
            let mut scope = BindingScope {
                state: &state,
                settings: &settings,
                relay_node_id: "node-1",
                control_url: remote_host_protocol::relay::control_url(&settings.relay_url),
                shutdown: &scope_shutdown,
                timing: ControlTiming::FAST,
                carrier: &mut runtime,
            };
            run_binding(&mut scope, &mut None).await;
            runtime.stop().await;
        });

        for _flap in 0..2 {
            let mut control = c.accept().await;
            assert_eq!(read_hello(&mut control).await.direct, UDP_CAPABILITY);
            control.send(WsMessage::Close(None)).await.unwrap();
        }
        let mut control = c.accept().await;
        assert_eq!(read_hello(&mut control).await.direct, UDP_CAPABILITY);
        assert!(
            !scope.is_finished(),
            "a relay close ended the binding scope"
        );

        shutdown.trigger();
        tokio::time::timeout(STEP_TIMEOUT, scope)
            .await
            .expect("the scope ends on shutdown")
            .expect("the scope task did not panic");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_binding_scope_holds_its_carrier_runtime_from_first_hello_to_revoke() {
        let mut c = MockC::start().await;
        let udp = UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let carrier = RuntimeCarrierConfig {
            udp: Some(FamilyBinds {
                ipv4: Some(udp),
                ipv6: None,
            }),
        };
        let (tg, shutdown, manager) = start_manager(&c, carrier).await;

        // The runtime is bound before control connects, and the hello says so.
        let mut first = c.accept().await;
        assert_eq!(read_hello(&mut first).await.direct, UDP_CAPABILITY);
        assert!(port_is_held(udp));

        // An offer is answered on the connection that delivered it.
        let answered = PunchId::generate();
        send_offer(&mut first, answered).await;
        assert_eq!(
            next_report(&mut first).await,
            ControlReport::DirectDeclined { punch_id: answered }
        );
        // One delivered as that connection closes is never answered on the next.
        send_offer(&mut first, PunchId::generate()).await;
        first.send(WsMessage::Close(None)).await.unwrap();
        drop(first);

        // Across the redial the runtime keeps its socket: it is never released.
        let mut second = tokio::time::timeout(STEP_TIMEOUT, async {
            loop {
                assert!(port_is_held(udp), "the flap restarted the carrier runtime");
                tokio::select! {
                    accepted = c.connections.recv() => break accepted.expect("mock C is serving"),
                    _ = tokio::time::sleep(Duration::from_millis(2)) => {}
                }
            }
        })
        .await
        .expect("the gateway redials control");
        assert_eq!(read_hello(&mut second).await.direct, UDP_CAPABILITY);
        let redialed = PunchId::generate();
        send_offer(&mut second, redialed).await;
        assert_eq!(
            next_report(&mut second).await,
            ControlReport::DirectDeclined { punch_id: redialed }
        );
        assert!(port_is_held(udp));

        // A revoke ends the scope: control closes, and the runtime stops and
        // lets go of its socket.
        tg.deps.stores.device.revoke(DEVICE_ID).await.unwrap();
        assert_eq!(next_binary(&mut second).await, None);
        tokio::time::timeout(STEP_TIMEOUT, async {
            while port_is_held(udp) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the stopped runtime releases its socket");

        stop_manager(shutdown, manager).await;
    }

    /// A secret store whose first reads of the gateway's static key fail, as
    /// a vault that is briefly unreadable does.
    struct StaticKeyUnreadable {
        inner: Arc<dyn SecretStore>,
        failures: AtomicUsize,
        reads: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl SecretStore for StaticKeyUnreadable {
        async fn store(&self, name: &str, encrypted_value: &[u8]) -> SecretResult<()> {
            self.inner.store(name, encrypted_value).await
        }

        async fn retrieve(&self, name: &str) -> SecretResult<Option<Vec<u8>>> {
            if name == NOISE_STATIC_VAULT_KEY {
                self.reads.fetch_add(1, Ordering::SeqCst);
                let failing = self
                    .failures
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                        left.checked_sub(1)
                    })
                    .is_ok();
                if failing {
                    return Err(StorageError::Storage("the vault is unreadable".to_owned()));
                }
            }
            self.inner.retrieve(name).await
        }

        async fn list(&self) -> SecretResult<Vec<String>> {
            self.inner.list().await
        }

        async fn delete(&self, name: &str) -> SecretResult<()> {
            self.inner.delete(name).await
        }

        async fn rewrite_all(&self, entries: &[(String, Vec<u8>)]) -> SecretResult<()> {
            self.inner.rewrite_all(entries).await
        }

        fn identity(&self) -> StoreIdentity {
            self.inner.identity()
        }
    }

    /// A static key the vault cannot read yet delays control rather than
    /// leaving the binding without direct carriers: the first hello, once
    /// the key reads, already carries the capability.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unreadable_static_key_is_retried_before_control_connects() {
        const UNREADABLE_READS: usize = 2;
        let mut c = MockC::start().await;
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        tg.deps
            .stores
            .device
            .create(&device_row(c.relay_url()))
            .await
            .expect("seed the approved device");
        let store = Arc::new(StaticKeyUnreadable {
            inner: tg.deps.stores.secret.clone(),
            failures: AtomicUsize::new(UNREADABLE_READS),
            reads: AtomicUsize::new(0),
        });
        let mut state = WsChannelState::from_deps(&tg.deps);
        state.secret_vault = Arc::new(SecretVault::new(
            tg.deps.secret_vault.master_key().clone(),
            Arc::clone(&store) as Arc<dyn SecretStore>,
        ));
        let carrier = RuntimeCarrierConfig {
            udp: Some(FamilyBinds {
                ipv4: Some("127.0.0.1:0".parse().unwrap()),
                ipv6: None,
            }),
        };
        let shutdown = ShutdownSignal::new();
        let manager = tokio::spawn(run(state, carrier, shutdown.clone(), ControlTiming::FAST));

        let mut control = c.accept().await;
        assert!(store.reads.load(Ordering::SeqCst) > UNREADABLE_READS);
        assert_eq!(read_hello(&mut control).await.direct, UDP_CAPABILITY);
        stop_manager(shutdown, manager).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_binding_without_direct_sockets_sends_no_capability() {
        let mut c = MockC::start().await;
        let (_tg, shutdown, manager) = start_manager(&c, no_carriers()).await;
        let mut control = c.accept().await;
        assert_eq!(read_hello(&mut control).await.direct, None);
        stop_manager(shutdown, manager).await;
    }
}
