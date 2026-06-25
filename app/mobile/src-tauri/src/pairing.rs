//! The pairing command's transport: dial the gateway's `/v1/device/pair` WS and
//! drive the SPAKE2 + mutual-confirm handshake through
//! `baybo_mobile_core::PairingClient`.
//!
//! The handshake pauses for a human decision, so it is split across two Tauri
//! commands: [`pair_begin`] runs up to the confirmation step and returns the
//! Bluetooth-style code for the UI to show; [`pair_confirm`] sends the user's
//! decision and finishes. The in-flight session (live WS + client) is parked in
//! [`PairingSessions`] between the two, keyed by the device id.
//!
//! The crypto + state machine live in the host-tested core; this file is just
//! the WebSocket pump (msgpack `PairFrame`s as binary frames) + the bits the
//! shell owns: generating the device's Noise keypair and a device id.

use std::collections::HashMap;

use baybo_mobile_core::{PairingClient, PairingRequest};
use device_proto::noise::StaticKeypair;
use device_proto::pairing::{self, ApnsEnv, PairFrame};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

/// In-flight pairing sessions, parked between `pair_begin` and `pair_confirm`
/// (keyed by device id). The map lock is held only to insert/remove — the WS
/// I/O runs on the owned session, never under the lock.
#[derive(Default)]
pub struct PairingSessions(Mutex<HashMap<String, PairSession>>);

struct PairSession {
    ws: Ws,
    client: PairingClient,
    device_id: String,
    /// The app's Noise static identity, carried from `pair_begin` so
    /// `pair_confirm` can persist its secret for content sessions.
    keypair: StaticKeypair,
}

/// The durable record persisted after pairing — everything the app needs to
/// reconnect and open a content session after a relaunch. Serialized into the
/// App Group keychain (`keychain::store_paired_record`).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PairedRecord {
    pub(crate) user_id: String,
    pub(crate) device_id: String,
    pub(crate) auth_token: String,
    pub(crate) gateway_static_pubkey: [u8; 32],
    pub(crate) direct_candidates: Vec<String>,
    pub(crate) relay_node_id: String,
    /// Base WS URL of the blind relay (C); empty when relay is off. Used to dial
    /// a relay content leg when every direct candidate fails.
    #[serde(default)]
    pub(crate) relay_url: String,
    /// The app's long-term Noise static identity (its content-session identity):
    /// the secret drives the IK initiator; the public half completes the keypair
    /// `device-proto` expects (snow re-derives it from the secret regardless).
    pub(crate) noise_secret: [u8; 32],
    #[serde(default)]
    pub(crate) noise_public: [u8; 32],
}

/// Load and decode the persisted pairing record, if the app is paired.
pub(crate) fn load_paired_record() -> Result<Option<PairedRecord>, String> {
    let Some(bytes) = crate::keychain::read_paired_record()? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| format!("decode paired record: {e}"))
}

/// The IPC DTOs ([`PairChallenge`] / [`PairedSummary`]) live in
/// baybo-mobile-core beside [`PairedGateway`], where ts-rs generates their TS
/// (core's `ts-export` feature). Re-exported so the commands + lib.rs keep
/// referring to them as `pairing::*`.
pub use baybo_mobile_core::{PairChallenge, PairedSummary};

/// Phase 1: dial `endpoint`, run SPAKE2 + send the sealed `DeviceHello`, and
/// return the confirmation code for the user to compare against the operator's
/// terminal. The live session is parked for [`pair_confirm`].
pub async fn pair_begin(
    sessions: &PairingSessions,
    endpoint: &str,
    code: &str,
    label: &str,
    relay: bool,
) -> Result<PairChallenge, String> {
    let base = endpoint.trim_end_matches('/');
    // Relay: join the rendezvous keyed by the code (`/pair/join/{code}`); the
    // gateway hosts the other leg. Direct: dial the gateway's pairing route.
    let url = if relay {
        remote_host_protocol::relay::pair_join_url(base, code)
    } else {
        format!("{base}/v1/device/pair")
    };
    let mut ws = connect_pair(&url, relay).await?;

    // The app's long-term Noise identity. TODO(persist): the secret belongs in
    // the keychain so content sessions reuse it across launches.
    let keypair = StaticKeypair::generate().map_err(|e| e.to_string())?;
    let device_id = format!("ios-{}", hex::encode(&keypair.public()[..8]));

    let req = PairingRequest {
        code: code.to_string(),
        device_id: device_id.clone(),
        label: label.to_string(),
        static_pubkey: keypair.public(),
        // Captured from `didRegisterForRemoteNotifications` (registration kicks
        // off at launch, so by pairing time the token is normally ready); empty
        // if it hasn't arrived yet, which the gateway re-registers out of band.
        apns_token: crate::push_register::apns_token().unwrap_or_default(),
        // A debug build's token is issued by the APNs sandbox; release/TestFlight
        // by production. The gateway tracks this per-device to pick the right host.
        apns_env: if cfg!(debug_assertions) {
            ApnsEnv::Sandbox
        } else {
            ApnsEnv::Production
        },
    };

    let (mut client, hello) = PairingClient::start(req);
    send(&mut ws, &hello).await?;

    let PairFrame::PakeReply { pake } = recv(&mut ws).await? else {
        return Err("expected PakeReply".into());
    };
    let sealed = client.on_pake_reply(&pake).map_err(|e| e.to_string())?;
    send(&mut ws, &sealed).await?;

    let confirm_code = client
        .confirm_code()
        .ok_or("confirmation code unavailable")?
        .to_string();

    sessions.0.lock().await.insert(
        device_id.clone(),
        PairSession {
            ws,
            client,
            device_id: device_id.clone(),
            keypair,
        },
    );

    Ok(PairChallenge {
        device_id,
        confirm_code,
    })
}

/// Phase 2: send the user's decision. On accept (and once the operator also
/// confirms) the gateway returns the sealed welcome and pairing finalizes; on
/// decline the gateway aborts and this returns an error.
pub async fn pair_confirm(
    sessions: &PairingSessions,
    device_id: &str,
    accepted: bool,
) -> Result<PairedSummary, String> {
    let mut session = sessions
        .0
        .lock()
        .await
        .remove(device_id)
        .ok_or("no pending pairing session for this device")?;

    let confirm = session
        .client
        .confirm(accepted)
        .map_err(|e| e.to_string())?;
    send(&mut session.ws, &confirm).await?;
    if !accepted {
        return Err("pairing cancelled".into());
    }

    let PairFrame::Sealed { nonce, ciphertext } = recv(&mut session.ws).await? else {
        return Err("expected sealed GatewayWelcome".into());
    };
    let paired = session
        .client
        .on_welcome(&nonce, &ciphertext)
        .map_err(|e| e.to_string())?;

    // Persist the push key to the shared App Group keychain so the NSE can
    // decrypt lock-screen previews (account `baybo.push-key.<device_id>`, since
    // the gateway stamps `bid = device_id` into every push payload).
    crate::keychain::store_push_key(&session.device_id, &paired.push_key)
        .map_err(|e| format!("persist push key: {e}"))?;

    // Persist the content-session record (auth token, gateway static key,
    // routing candidates, and the app's Noise secret) so the app can reconnect
    // and open a content session after a relaunch.
    let record = PairedRecord {
        user_id: paired.user_id.clone(),
        device_id: session.device_id.clone(),
        auth_token: paired.auth_token.clone(),
        gateway_static_pubkey: paired.gateway_static_pubkey,
        direct_candidates: paired.direct_candidates.clone(),
        relay_node_id: paired.relay_node_id.clone(),
        relay_url: paired.relay_url.clone(),
        noise_secret: session.keypair.secret(),
        noise_public: session.keypair.public(),
    };
    let bytes = serde_json::to_vec(&record).map_err(|e| format!("encode paired record: {e}"))?;
    crate::keychain::store_paired_record(&bytes)
        .map_err(|e| format!("persist paired record: {e}"))?;

    Ok(PairedSummary::from(&paired))
}

/// The owning user of the persisted pairing, if the app is already paired
/// (so a relaunch can skip straight to "connected" instead of the scan form).
pub fn paired_user() -> Option<String> {
    load_paired_record().ok().flatten().map(|r| r.user_id)
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Dial the pairing WS. On the relay path the gateway's host leg may not be
/// parked the instant the app scans, so retry briefly on connect failure.
async fn connect_pair(url: &str, relay: bool) -> Result<Ws, String> {
    let attempts = if relay { 30 } else { 1 };
    let mut last = String::new();
    for i in 0..attempts {
        match connect_async(url).await {
            Ok((ws, _)) => return Ok(ws),
            Err(e) => {
                last = format!("connect {url}: {e}");
                if i + 1 < attempts {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }
    }
    Err(last)
}

async fn send(ws: &mut Ws, frame: &PairFrame) -> Result<(), String> {
    let bytes = pairing::encode(frame).map_err(|e| format!("encode: {e}"))?;
    ws.send(Message::Binary(bytes))
        .await
        .map_err(|e| format!("send: {e}"))
}

async fn recv(ws: &mut Ws) -> Result<PairFrame, String> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Binary(b))) => {
                return pairing::decode(&b).map_err(|e| format!("decode: {e}"));
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            Some(Ok(Message::Close(_))) | None => return Err("connection closed".into()),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(format!("ws: {e}")),
        }
    }
}
