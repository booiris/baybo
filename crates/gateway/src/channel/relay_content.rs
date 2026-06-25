//! The gateway's relay-**content** control side.
//!
//! A NAT'd gateway can't be dialed, so for post-pairing chat it holds a
//! persistent outbound **control connection** to C (`/control`). When a phone
//! arrives at the relay for this gateway's `relay_node_id`, C pushes
//! [`ControlSignal::OpenDataLeg`]; the gateway dials a data leg
//! (`/content/host/{relay_key}`) and runs the Noise content responder over it —
//! byte-identical to a direct `/v1/device/content` session, only the transport
//! differs (it reuses [`super::device_content::run_content_over_relay`]).
//!
//! The connection is held for the process's life and re-dialed with a fixed
//! backoff after any drop, so a phone can always reach a live gateway via C.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use super::device_content::run_content_over_relay;
use super::state::WsChannelState;
use remote_host_protocol::relay::INSTANCE_KEY_HEADER;

use crate::relay::{ControlHello, ControlSignal, connect_control, load_or_create_relay_node_id};

/// Backoff between control-connection (re)dials.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(5);

/// Config for the relay-content control connection (the same relay block the
/// pairing host uses).
#[derive(Debug, Clone)]
pub(crate) struct RelayContentConfig {
    pub relay_url: String,
    pub instance_key: String,
}

/// Spawn the relay-content control manager. Runs until the process exits.
pub(crate) fn spawn(state: WsChannelState, config: RelayContentConfig) {
    tracing::info!(relay = %config.relay_url, "relay-content: control manager started");
    tokio::spawn(run(state, config));
}

async fn run(state: WsChannelState, config: RelayContentConfig) {
    let relay_node_id = match load_or_create_relay_node_id(&state.secret_vault).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "relay-content: no relay_node_id; control disabled");
            return;
        }
    };
    let control_url = remote_host_protocol::relay::control_url(&config.relay_url);
    loop {
        if let Err(e) = run_once(&state, &config, &control_url, &relay_node_id).await {
            tracing::debug!(error = %e, "relay-content: control connection ended");
        }
        tokio::time::sleep(RECONNECT_BACKOFF).await;
    }
}

/// Hold one control connection until it closes, opening a content data leg for
/// each `OpenDataLeg` signal C pushes.
async fn run_once(
    state: &WsChannelState,
    config: &RelayContentConfig,
    control_url: &str,
    relay_node_id: &str,
) -> Result<(), String> {
    let (tx, mut rx) = mpsc::channel::<ControlSignal>(32);
    let pump = tokio::spawn({
        let hello = ControlHello {
            relay_node_id: relay_node_id.to_owned(),
            instance_key: config.instance_key.clone(),
        };
        let control_url = control_url.to_owned();
        async move { connect_control(&control_url, &hello, tx).await }
    });

    while let Some(ControlSignal::OpenDataLeg { relay_key }) = rx.recv().await {
        let state = state.clone();
        let config = config.clone();
        tokio::spawn(async move {
            open_data_leg(&state, &config, &relay_key).await;
        });
    }

    // The control connection closed (the pump dropped `tx`); surface its result.
    match pump.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(e) => Err(format!("control task panicked: {e}")),
    }
}

/// Dial a content data leg for `relay_key` and run the Noise content responder.
async fn open_data_leg(state: &WsChannelState, config: &RelayContentConfig, relay_key: &str) {
    let url = remote_host_protocol::relay::content_host_url(&config.relay_url, relay_key);
    let mut req = match url.into_client_request() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "relay-content: bad data-leg url");
            return;
        }
    };
    match config.instance_key.parse() {
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
