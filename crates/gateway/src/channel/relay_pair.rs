//! The relay **host leg** the operator's `baybo device pair` opens.
//!
//! Pairing always runs through the operator's relay (C): `baybo device pair`
//! opens an authenticated host leg on the relay (`/pair/host/{rendezvous_id}`,
//! gated by its admission key) and runs the XXpsk0 + mutual-confirm [`drive`]
//! over it. The relay blindly pipes the app's `/pair/join/{rendezvous_id}` leg to
//! ours, so the handshake is byte-identical to a local socket — only the
//! transport differs. There is no daemon-served pairing path; the gateway daemon
//! only serves *content* for already-paired devices (see [`super::relay_content`]).
//!
//! A host leg whose app never arrives times out and is re-opened, so a live
//! rendezvous is continuously hosted until it pairs or the operator's command
//! exits. Because the rendezvous id is **public**, anyone who learns it (the
//! relay, a QR photographer, any holder of the shared admission key) can join and
//! fail the PSK handshake to grief pairing — so on a handshake abort (as opposed
//! to a transport/connect error) we **re-park immediately**, with no backoff, so
//! a leg-stealer can't impose latency on the real app's retry.

use std::time::Duration;

use device_proto::pairing::{self, PairFrame};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use remote_host_protocol::relay::REMOTE_API_KEY_HEADER;

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

/// How a single hosted leg ended — distinguishing a transport/connect failure
/// (back off before retrying) from a handshake abort (re-park immediately so a
/// PSK-failing leg-stealer can't delay the real app).
enum LegEnd {
    /// The handshake completed — pairing is done.
    Done,
    /// A bad URL/key or a failed connect, before the handshake. Back off.
    Connect(String),
    /// The leg matched and the handshake ran but aborted (PSK mismatch, a
    /// decline, a timeout, …). It was best-effort `Reject`ed.
    Handshake(String),
}

/// Open a single `/pair/host/{rendezvous_id}` leg authenticated by `remote_api_key`
/// and run one pairing handshake over it.
async fn host_leg_once(
    deps: &PairingHostDeps,
    relay_url: &str,
    remote_api_key: &str,
    rendezvous_id: &str,
) -> LegEnd {
    let url = remote_host_protocol::relay::pair_host_url(relay_url, rendezvous_id);
    let req = match url.into_client_request() {
        Ok(mut req) => match remote_api_key.parse() {
            Ok(value) => {
                req.headers_mut().insert(REMOTE_API_KEY_HEADER, value);
                req
            }
            Err(e) => return LegEnd::Connect(format!("bad instance key header: {e}")),
        },
        Err(e) => return LegEnd::Connect(format!("bad relay url: {e}")),
    };
    let ws = match connect_async(req).await {
        Ok((ws, _)) => ws,
        Err(e) => return LegEnd::Connect(format!("host leg connect failed: {e}")),
    };
    let mut transport = RelayLegTransport(ws);
    match drive(&mut transport, deps).await {
        Ok(()) => LegEnd::Done,
        Err(reason) => {
            let _ = transport
                .send_frame(&PairFrame::Reject {
                    reason: reason.clone(),
                })
                .await;
            LegEnd::Handshake(reason)
        }
    }
}

/// Backoff before re-opening a host leg after a *connect/transport* failure (the
/// relay may be momentarily unreachable). Handshake aborts do **not** back off —
/// see [`host_pairing_leg`].
const REHOST_BACKOFF: Duration = Duration::from_millis(500);

/// Host `/pair/host/{rendezvous_id}` on `relay_url` with `remote_api_key` and run
/// the pairing handshake, re-opening the leg while the slot stays live (the
/// operator may not have shown the QR yet, or a leg may time out / be stolen).
/// Returns `Ok(())` once a handshake completes, or `Err` once the slot is gone
/// (paired / aged out) or a side declined. Driven by `baybo device pair`; the
/// caller aborts the task when its operator flow ends.
pub async fn host_pairing_leg(
    deps: &PairingHostDeps,
    relay_url: &str,
    remote_api_key: &str,
    rendezvous_id: &str,
) -> Result<(), String> {
    // `baybo device pair` calls this from the CLI process — outside the gateway
    // daemon (whose own dialer installs the provider in `relay_content::spawn`) —
    // and our graph enables both aws-lc-rs and ring, so install one before the wss
    // dial below or connect_async panics. Idempotent: Err means one is already
    // installed.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    loop {
        // Keep whether this was a handshake abort (re-park now) or a connect
        // failure (back off) alongside the reason, so there is one source of truth.
        let (reason, handshake_abort) =
            match host_leg_once(deps, relay_url, remote_api_key, rendezvous_id).await {
                LegEnd::Done => return Ok(()),
                LegEnd::Handshake(reason) => (reason, true),
                LegEnd::Connect(reason) => (reason, false),
            };

        // Stop re-hosting once there's a terminal outcome: the slot is gone (a
        // successful pair consumed it, or it aged out) or either side has declined
        // — re-opening then would just strand a connection that immediately
        // rejects, and (on a decline) we want this leg to finish *after* having
        // delivered its `Reject`, so the caller can await it.
        let terminal = match deps
            .device_pairing
            .claim_slot(rendezvous_id)
            .await
            .map_err(|e| format!("poll slot: {e}"))?
        {
            None => true,
            Some(slot) => {
                slot.operator_decision == Some(false) || slot.device_decision == Some(false)
            }
        };
        if terminal {
            return Err(reason);
        }

        // A PSK-auth/handshake abort with a still-live slot means a leg-stealer
        // (or a transient app glitch) failed the handshake — re-park immediately,
        // no backoff, so the real app's retry can match without delay. Anything
        // else (an unreachable relay) is pointless to hammer, so back off first.
        if !handshake_abort {
            tokio::time::sleep(REHOST_BACKOFF).await;
        }
    }
}
