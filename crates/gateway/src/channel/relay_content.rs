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

use super::device_content::run_content_over_relay;
use super::state::{LegDedup, WsChannelState};
use remote_host_protocol::relay::REMOTE_API_KEY_HEADER;

use crate::relay::{ControlHello, ControlSignal, connect_control, load_or_create_relay_node_id};

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

/// The relay endpoint + admission key the gateway dials, resolved from the single
/// approved device row.
struct RelaySettings {
    relay_url: String,
    remote_api_key: String,
}

/// Resolve the relay settings from the approved device row (one gateway = one
/// app). `None` when no device is paired, or its row predates the recorded relay
/// fields (empty — re-pair to populate).
async fn approved_relay_settings(state: &WsChannelState) -> Option<RelaySettings> {
    let row = state
        .device_store
        .list(Some(DeviceStatus::Approved))
        .await
        .ok()?
        .into_iter()
        .next()?;
    if row.relay_url.is_empty() || row.remote_api_key.is_empty() {
        return None;
    }
    Some(RelaySettings {
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
    let relay_node_id = match load_or_create_relay_node_id(&state.secret_vault).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "relay-content: no relay_node_id; control disabled");
            return;
        }
    };
    loop {
        if shutdown.is_shutdown() {
            break;
        }
        // Idle until a device is paired (and has recorded its relay settings),
        // but wake immediately on shutdown rather than after the full poll tick.
        let Some(settings) = approved_relay_settings(&state).await else {
            tokio::select! {
                _ = tokio::time::sleep(DEVICE_POLL_INTERVAL) => {}
                _ = shutdown.wait() => break,
            }
            continue;
        };
        let control_url = remote_host_protocol::relay::control_url(&settings.relay_url);
        tracing::info!(
            relay = %settings.relay_url,
            "relay-content: device paired; holding control connection"
        );
        // `run_once` owns its child tasks (the control pump + per-signal data
        // legs) and drains them on return, so it is *not* wrapped in a cancelling
        // select! here — it handles `shutdown` internally and returns cleanly.
        if let Err(e) = run_once(&state, &settings, &control_url, &relay_node_id, &shutdown).await {
            tracing::debug!(error = %e, "relay-content: control connection ended");
        }
        if shutdown.is_shutdown() {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(reconnect_delay()) => {}
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
) -> Result<(), String> {
    let (tx, mut rx) = mpsc::channel::<ControlSignal>(32);
    let pump = tokio::spawn({
        let hello = ControlHello {
            relay_node_id: relay_node_id.to_owned(),
        };
        let control_url = control_url.to_owned();
        let remote_api_key = settings.remote_api_key.clone();
        async move { connect_control(&control_url, &remote_api_key, &hello, tx).await }
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
                Some(ControlSignal::OpenDataLeg { relay_key }) => {
                    let state = state.clone();
                    let relay_url = settings.relay_url.clone();
                    let remote_api_key = settings.remote_api_key.clone();
                    // Hand the leg its own AbortHandle over a oneshot so the content
                    // session can register it in the device-dedup registry once Noise
                    // resolves the device_id — and a newer leg can abort this one.
                    let (ah_tx, ah_rx) = tokio::sync::oneshot::channel();
                    let abort = legs.spawn(async move {
                        open_data_leg(&state, &relay_url, &remote_api_key, &relay_key, ah_rx).await;
                    });
                    let _ = ah_tx.send(abort);
                }
                // The control connection closed (the pump dropped `tx`).
                None => break,
            },
            _ = poll.tick() => {
                if approved_relay_settings(state).await.is_none() {
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

    match pump.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(e) if pump_aborted && e.is_cancelled() => Ok(()),
        Err(e) => Err(format!("control task panicked: {e}")),
    }
}

/// Dial a content data leg for `relay_key` and run the Noise content responder.
/// `ah_rx` delivers this task's own [`AbortHandle`](tokio::task::AbortHandle) (sent
/// by the spawner), which the content session registers in the device-dedup
/// registry once it resolves the `device_id`.
async fn open_data_leg(
    state: &WsChannelState,
    relay_url: &str,
    remote_api_key: &str,
    relay_key: &str,
    ah_rx: tokio::sync::oneshot::Receiver<tokio::task::AbortHandle>,
) {
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
            tracing::debug!(error = %e, "relay-content: data-leg connect failed");
            return;
        }
    };
    // Learn our own AbortHandle (sent immediately after spawn) so the session can
    // dedup this device's legs; if it never arrives, run without dedup.
    let dedup = ah_rx.await.ok().map(|abort| LegDedup {
        registry: state.device_leg_registry.clone(),
        abort,
    });
    run_content_over_relay(ws, state, dedup).await;
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
