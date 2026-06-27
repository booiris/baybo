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
//! pairing ([`baybo_store::DeviceRow::relay_url`] / `instance_key`), re-dialing
//! with a fixed backoff after any drop. When the device is revoked the control
//! connection is torn down promptly, so the gateway stops advertising a route it
//! can no longer authenticate a content session for.

use std::time::Duration;

use baybo_store::DeviceStatus;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::device_content::run_content_over_relay;
use super::state::WsChannelState;
use remote_host_protocol::relay::INSTANCE_KEY_HEADER;

use crate::relay::{ControlHello, ControlSignal, connect_control, load_or_create_relay_node_id};

/// Backoff between control-connection (re)dials.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(5);

/// Poll cadence for the approved device row — both while idle (waiting for a
/// pairing) and while a control connection is live (watching for a revoke).
/// Cheap: one tiny libsql read per tick against a ≤1-row table.
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// The relay endpoint + admission key the gateway dials, resolved from the single
/// approved device row.
struct RelaySettings {
    relay_url: String,
    instance_key: String,
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
    if row.relay_url.is_empty() || row.instance_key.is_empty() {
        return None;
    }
    Some(RelaySettings {
        relay_url: row.relay_url,
        instance_key: row.instance_key,
    })
}

/// Spawn the relay-content control manager. Runs until the process exits, idling
/// until a device is paired.
pub(crate) fn spawn(state: WsChannelState) {
    // The control connection dials `wss://` via tokio-tungstenite, which uses
    // rustls's process-default CryptoProvider. Our graph enables both aws-lc-rs
    // and ring, so install aws-lc-rs explicitly before the first dial or
    // connect_async panics. Idempotent — Err means one is already installed.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    tracing::info!("relay-content: control manager started");
    tokio::spawn(run(state));
}

async fn run(state: WsChannelState) {
    let relay_node_id = match load_or_create_relay_node_id(&state.secret_vault).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "relay-content: no relay_node_id; control disabled");
            return;
        }
    };
    loop {
        // Idle until a device is paired (and has recorded its relay settings).
        let Some(settings) = approved_relay_settings(&state).await else {
            tokio::time::sleep(DEVICE_POLL_INTERVAL).await;
            continue;
        };
        let control_url = remote_host_protocol::relay::control_url(&settings.relay_url);
        tracing::info!(
            relay = %settings.relay_url,
            "relay-content: device paired; holding control connection"
        );
        if let Err(e) = run_once(&state, &settings, &control_url, &relay_node_id).await {
            tracing::debug!(error = %e, "relay-content: control connection ended");
        }
        tokio::time::sleep(RECONNECT_BACKOFF).await;
    }
}

/// Hold one control connection until it closes (or the device is revoked),
/// opening a content data leg for each `OpenDataLeg` signal C pushes. Polls the
/// device row alongside so a revoke tears the connection down rather than letting
/// it linger.
async fn run_once(
    state: &WsChannelState,
    settings: &RelaySettings,
    control_url: &str,
    relay_node_id: &str,
) -> Result<(), String> {
    let (tx, mut rx) = mpsc::channel::<ControlSignal>(32);
    let pump = tokio::spawn({
        let hello = ControlHello {
            relay_node_id: relay_node_id.to_owned(),
        };
        let control_url = control_url.to_owned();
        let instance_key = settings.instance_key.clone();
        async move { connect_control(&control_url, &instance_key, &hello, tx).await }
    });

    let mut poll = tokio::time::interval(DEVICE_POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    poll.tick().await; // the first tick fires immediately; skip it

    // `true` once we abort the pump ourselves (device revoked), so its
    // cancellation JoinError isn't surfaced as a connection error.
    let mut revoked = false;
    loop {
        tokio::select! {
            signal = rx.recv() => match signal {
                Some(ControlSignal::OpenDataLeg { relay_key }) => {
                    let state = state.clone();
                    let relay_url = settings.relay_url.clone();
                    let instance_key = settings.instance_key.clone();
                    tokio::spawn(async move {
                        open_data_leg(&state, &relay_url, &instance_key, &relay_key).await;
                    });
                }
                // The control connection closed (the pump dropped `tx`).
                None => break,
            },
            _ = poll.tick() => {
                if approved_relay_settings(state).await.is_none() {
                    pump.abort();
                    revoked = true;
                    break;
                }
            }
        }
    }

    match pump.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(e) if revoked && e.is_cancelled() => Ok(()),
        Err(e) => Err(format!("control task panicked: {e}")),
    }
}

/// Dial a content data leg for `relay_key` and run the Noise content responder.
async fn open_data_leg(
    state: &WsChannelState,
    relay_url: &str,
    instance_key: &str,
    relay_key: &str,
) {
    let url = remote_host_protocol::relay::content_host_url(relay_url, relay_key);
    let mut req = match url.into_client_request() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "relay-content: bad data-leg url");
            return;
        }
    };
    match instance_key.parse() {
        Ok(v) => {
            req.headers_mut().insert(INSTANCE_KEY_HEADER, v);
        }
        Err(e) => {
            tracing::warn!(error = %e, "relay-content: bad instance key header");
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
    run_content_over_relay(ws, state).await;
}
