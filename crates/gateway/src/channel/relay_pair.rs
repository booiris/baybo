//! The gateway's relay-pairing **host** side.
//!
//! Phase-1 pairing is a direct dial to `/v1/device/pair`. This module lets a
//! phone behind a different network pair through the operator's relay (C): for
//! each live pairing slot, the gateway opens an authenticated **host leg** on
//! the relay (`/pair/host/{code}`, gated by its admission key) and runs the
//! exact same SPAKE2 + mutual-confirm [`drive`] over it. The relay blindly pipes
//! the app's `/pair/join/{code}` leg to ours, so the handshake is byte-identical
//! to the direct path — only the transport differs.
//!
//! A host leg whose app never arrives times out and is re-opened on the next
//! poll, so a live slot is continuously hosted until it pairs or ages out.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use device_proto::pairing::{self, PairFrame};
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use remote_host_protocol::relay::INSTANCE_KEY_HEADER;

use super::device_pair::{PairTransport, PairingHostDeps, drive};
use super::state::WsChannelState;

/// How often the manager scans for live slots that need a host leg.
const HOST_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Config for hosting pairing legs on the relay.
#[derive(Debug, Clone)]
pub(crate) struct RelayPairConfig {
    /// Base WS URL of the relay (e.g. `wss://proxy.baybo.space`).
    pub relay_url: String,
    /// The gateway's admission key (a `RELAY_INSTANCE_KEYS` entry on the relay).
    pub instance_key: String,
}

type RelayWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A relay host leg as a [`PairTransport`], so [`drive`] runs over it unchanged.
struct RelayLegTransport(RelayWs);

#[async_trait::async_trait]
impl PairTransport for RelayLegTransport {
    async fn recv_frame(&mut self, timeout: Duration) -> Result<PairFrame, String> {
        loop {
            let next = tokio::time::timeout(timeout, self.0.next())
                .await
                .map_err(|_| "timed out waiting for pairing frame".to_string())?;
            match next {
                Some(Ok(Message::Binary(b))) => {
                    return pairing::decode(&b).map_err(|e| format!("decode pairing frame: {e}"));
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                Some(Ok(Message::Close(_))) | None => return Err("relay leg closed".into()),
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(format!("relay ws error: {e}")),
            }
        }
    }

    async fn send_frame(&mut self, frame: &PairFrame) -> Result<(), String> {
        let bytes = pairing::encode(frame).map_err(|e| format!("encode pairing frame: {e}"))?;
        self.0
            .send(Message::Binary(bytes))
            .await
            .map_err(|e| format!("send pairing frame: {e}"))
    }
}

/// Spawn the relay-pairing host manager. It runs until the process exits.
pub(crate) fn spawn(state: WsChannelState, config: RelayPairConfig) {
    tracing::info!(relay = %config.relay_url, "relay-pair: host manager started");
    tokio::spawn(run(state, config));
}

async fn run(state: WsChannelState, config: RelayPairConfig) {
    // Codes with a host task currently running, so the poll doesn't double-host.
    let hosting: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    loop {
        match state.device_pairing.list_slots().await {
            Ok(slots) => {
                let now = chrono::Utc::now().timestamp();
                for slot in slots.into_iter().filter(|s| !s.is_expired(now)) {
                    let code = slot.code;
                    {
                        let mut h = hosting.lock();
                        if !h.insert(code.clone()) {
                            continue; // already hosting this code
                        }
                    }
                    let state = state.clone();
                    let config = config.clone();
                    let hosting = Arc::clone(&hosting);
                    tokio::spawn(async move {
                        host_one(&state, &config, &code).await;
                        hosting.lock().remove(&code);
                    });
                }
            }
            Err(e) => tracing::warn!(error = %e, "relay-pair: list slots failed"),
        }
        tokio::time::sleep(HOST_POLL_INTERVAL).await;
    }
}

/// Open one host leg for `code` and run the handshake. Returns when the handshake
/// finishes, the app never shows (timeout), or the connection fails — the daemon
/// re-hosts on the next poll while the slot is still live.
async fn host_one(state: &WsChannelState, config: &RelayPairConfig, code: &str) {
    if let Err(reason) = host_leg_once(
        &state.pairing_host_deps(),
        &config.relay_url,
        &config.instance_key,
        code,
    )
    .await
    {
        tracing::debug!(reason = %reason, "relay-pair: host leg ended");
    }
}

/// Open a single `/pair/host/{code}` leg authenticated by `instance_key` and run
/// one pairing handshake over it. `Ok(())` on a completed handshake; `Err` on a
/// bad URL/key, a failed connect, or an aborted handshake (the app never showed,
/// declined, …) — on an abort the leg is best-effort `Reject`ed.
async fn host_leg_once(
    deps: &PairingHostDeps,
    relay_url: &str,
    instance_key: &str,
    code: &str,
) -> Result<(), String> {
    let url = remote_host_protocol::relay::pair_host_url(relay_url, code);
    let mut req = url
        .into_client_request()
        .map_err(|e| format!("bad relay url: {e}"))?;
    let value = instance_key
        .parse()
        .map_err(|e| format!("bad instance key header: {e}"))?;
    req.headers_mut().insert(INSTANCE_KEY_HEADER, value);
    let (ws, _) = connect_async(req)
        .await
        .map_err(|e| format!("host leg connect failed: {e}"))?;
    let mut transport = RelayLegTransport(ws);
    if let Err(reason) = drive(&mut transport, deps).await {
        let _ = transport
            .send_frame(&PairFrame::Reject {
                reason: reason.clone(),
            })
            .await;
        return Err(reason);
    }
    Ok(())
}

/// Backoff before re-opening a host leg whose app never arrived.
const REHOST_BACKOFF: Duration = Duration::from_millis(500);

/// Host `/pair/host/{code}` on `relay_url` with `instance_key` and run the pairing
/// handshake, re-opening the leg while the slot stays live (the operator may not
/// have shown the QR yet, or a leg may time out waiting for the app). Returns
/// `Ok(())` once a handshake completes, or `Err` once the slot is gone (paired
/// elsewhere / expired). This is the self-contained equivalent of the daemon's
/// poll-driven host manager for a single code, used by `baybo device pair`; the
/// caller aborts the task when its operator flow ends.
pub async fn host_pairing_leg(
    deps: &PairingHostDeps,
    relay_url: &str,
    instance_key: &str,
    code: &str,
) -> Result<(), String> {
    // `baybo device pair` calls this from the CLI process — outside the gateway
    // server, which installs the provider in GatewayServer::new — and our graph
    // enables both aws-lc-rs and ring, so install one before the wss dial below or
    // connect_async panics. Idempotent: Err means one is already installed.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    loop {
        match host_leg_once(deps, relay_url, instance_key, code).await {
            Ok(()) => return Ok(()),
            Err(reason) => {
                // Stop once the slot is gone (consumed by a successful pair or
                // aged out); else re-host so a late or retrying app still lands.
                let live = deps
                    .device_pairing
                    .claim_slot(code)
                    .await
                    .map_err(|e| format!("poll slot: {e}"))?
                    .is_some();
                if !live {
                    return Err(reason);
                }
                tokio::time::sleep(REHOST_BACKOFF).await;
            }
        }
    }
}
