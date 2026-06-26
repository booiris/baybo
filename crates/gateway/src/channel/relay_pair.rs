//! The relay **host leg** the operator's `baybo device pair` opens.
//!
//! Pairing always runs through the operator's relay (C): `baybo device pair`
//! opens an authenticated host leg on the relay (`/pair/host/{code}`, gated by
//! its admission key) and runs the SPAKE2 + mutual-confirm [`drive`] over it. The
//! relay blindly pipes the app's `/pair/join/{code}` leg to ours, so the
//! handshake is byte-identical to a local socket — only the transport differs.
//! There is no daemon-served pairing path; the gateway daemon only serves
//! *content* for already-paired devices (see [`super::relay_content`]).
//!
//! A host leg whose app never arrives times out and is re-opened, so a live code
//! is continuously hosted until it pairs or the operator's command exits.

use std::time::Duration;

use device_proto::pairing::{self, PairFrame};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use remote_host_protocol::relay::INSTANCE_KEY_HEADER;

use super::device_pair::{PairTransport, PairingHostDeps, drive};

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
/// `Ok(())` once a handshake completes, or `Err` once the slot is gone (paired /
/// aged out) or a side declined. Driven by `baybo device pair`; the caller aborts
/// the task when its operator flow ends.
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
                // Stop re-hosting once there's a terminal outcome: the slot is
                // gone (a successful pair consumed it, or it aged out) or either
                // side has declined — re-opening a leg then would just strand a
                // connection that immediately rejects, and (on a decline) we want
                // this leg to finish *after* having delivered its `Reject`, so the
                // caller can await it. Otherwise re-host so a late/retrying app
                // still lands.
                let done = match deps
                    .device_pairing
                    .claim_slot(code)
                    .await
                    .map_err(|e| format!("poll slot: {e}"))?
                {
                    None => true,
                    Some(slot) => {
                        slot.operator_decision == Some(false) || slot.device_decision == Some(false)
                    }
                };
                if done {
                    return Err(reason);
                }
                tokio::time::sleep(REHOST_BACKOFF).await;
            }
        }
    }
}
