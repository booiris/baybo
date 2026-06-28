//! The pairing command's transport: dial the gateway's `/v1/device/pair` WS and
//! drive the XXpsk0 + mutual-confirm handshake through
//! `baybo_mobile_core::PairingClient`.
//!
//! The handshake pauses for a human decision, so it is split across two Tauri
//! commands: [`pair_begin`] runs up to the confirmation step and returns the
//! Bluetooth-style code for the UI to show; [`pair_confirm`] sends the user's
//! decision and finishes. Between the two, the live WS is owned by a background
//! pump task ([`run_pair_pump`]) — keyed by device id in [`PairingSessions`] via
//! a [`PairControl`] handle — so the app keeps *listening* while the confirm
//! screen is up: if the gateway aborts (the operator declined, or the link
//! dropped) before the user taps, the pump pushes a [`PairAborted`] over the
//! `on_abort` channel and the UI dismisses the screen instead of hanging.
//!
//! The crypto + state machine live in the host-tested core; this file is just
//! the WebSocket pump (msgpack `PairFrame`s as binary frames) + the bits the
//! shell owns: loading-or-minting the device's persistent Noise keypair (and the
//! device id derived from it).

use std::collections::HashMap;
use std::time::Duration;

use baybo_mobile_core::{PairingClient, PairingRequest};
use device_proto::aead::KEY_LEN;
use device_proto::noise::StaticKeypair;
use device_proto::pairing::{self, ApnsEnv, PairFrame};
use device_proto::psk_pair::PairingSecret;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tokio::sync::{Mutex, oneshot};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

/// How long the decline path waits for the gateway to acknowledge our
/// `DeviceConfirm(false)` before dropping the socket — long enough for the relay
/// round-trip, so the operator's side reliably learns we cancelled.
const DECLINE_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait for the gateway's `HandshakeReply` before giving up. Without
/// it a malicious relay could keep the app pinned open indefinitely on the first
/// reply (a `msg2` that never arrives).
const HANDSHAKE_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// In-flight pairing sessions, parked between `pair_begin` and `pair_confirm`
/// (keyed by device id). The map lock is held only to insert/remove; the live WS
/// is owned by the background [`run_pair_pump`] task, reached through this handle.
#[derive(Default)]
pub struct PairingSessions(Mutex<HashMap<String, PairControl>>);

/// Control handle to a parked session's [`run_pair_pump`] task.
struct PairControl {
    /// Hands the user's Pair/Cancel decision to the pump.
    decision_tx: oneshot::Sender<bool>,
    /// Resolves with the pump's terminal outcome once the user has decided.
    outcome_rx: oneshot::Receiver<Result<PairedSummary, String>>,
}

/// The durable record persisted after pairing — everything the app needs to
/// reconnect and open a content session after a relaunch. Serialized into the
/// App Group keychain (`keychain::store_paired_record`).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PairedRecord {
    pub(crate) device_id: String,
    pub(crate) auth_token: String,
    pub(crate) gateway_static_pubkey: [u8; 32],
    pub(crate) relay_node_id: String,
    /// Base WS URL of the blind relay (C); empty when relay is off. Used to dial
    /// the relay content leg.
    #[serde(default)]
    pub(crate) relay_url: String,
    /// The relay admission key (the QR's `k`) this device presents on the relay
    /// content-join leg. Symmetric with the gateway's host-side key.
    #[serde(default)]
    pub(crate) remote_api_key: String,
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

/// The app's long-term Noise static identity. Loaded from the keychain so the
/// derived `device_id` is stable across re-pairings and launches; minted and
/// persisted on first use. On a desktop dev build (no on-device keychain) the
/// read is always empty, so each run mints an ephemeral key — fine off-device.
fn load_or_create_device_identity() -> Result<StaticKeypair, String> {
    if let Some((secret, public)) = crate::keychain::read_device_identity()? {
        return Ok(StaticKeypair::from_parts(public, secret));
    }
    let keypair = StaticKeypair::generate().map_err(|e| e.to_string())?;
    crate::keychain::store_device_identity(&keypair.secret(), &keypair.public())?;
    Ok(keypair)
}

/// The IPC DTOs ([`PairChallenge`] / [`PairedSummary`]) live in
/// baybo-mobile-core beside [`PairedGateway`], where ts-rs generates their TS
/// (core's `ts-export` feature). Re-exported so the commands + lib.rs keep
/// referring to them as `pairing::*`.
pub use baybo_mobile_core::{PairAborted, PairChallenge, PairedSummary};

/// Dial `endpoint`, run XXpsk0 through the `DeviceHello` step, and return the
/// confirmation code for the user to compare against the operator's terminal.
/// Hands the live session to a background [`run_pair_pump`] task that watches
/// for a gateway abort (pushing [`PairAborted`] over `on_abort`) and finishes the
/// handshake once the user decides via [`pair_confirm`].
pub async fn pair_begin(
    sessions: &PairingSessions,
    endpoint: &str,
    rendezvous_id: &str,
    secret: &str,
    remote_api_key: Option<String>,
    on_abort: Channel<PairAborted>,
) -> Result<PairChallenge, String> {
    let base = endpoint.trim_end_matches('/');
    // Pairing is relay-only: join the rendezvous keyed by the public rendezvous
    // id (`/pair/join/{rendezvous_id}`); the operator's `baybo device pair` hosts
    // the other leg on the relay.
    let url = remote_host_protocol::relay::pair_join_url(base, rendezvous_id);
    let mut ws = connect_pair(&url, remote_api_key.as_deref()).await?;

    // Decode the QR secret (the Noise PSK). It must be exactly 32 bytes — a
    // short/typeable secret is forbidden (offline-crackable by the relay).
    let secret = decode_secret(secret)?;

    // The app's long-term Noise identity, loaded from (or minted into) the
    // keychain so the derived device_id stays stable across re-pairings and
    // launches, and content sessions reuse the same static across relaunches.
    let keypair = load_or_create_device_identity()?;
    let device_id = format!("ios-{}", hex::encode(&keypair.public()[..8]));

    let req = PairingRequest {
        rendezvous_id: rendezvous_id.to_string(),
        secret,
        // Bound into the handshake prologue (anti-cross-binding); must match the
        // gateway's relay URL — it does, both being the QR `h=`.
        endpoint: endpoint.to_string(),
        device_id: device_id.clone(),
        // The app static rides the handshake as an XX token; the gateway learns
        // the public half in-band.
        static_secret: keypair.secret(),
        // Captured from `didRegisterForRemoteNotifications` (registration kicks
        // off at launch, so by pairing time the token is often ready); empty if
        // it has not arrived yet, in which case the paired app registers it
        // directly with C after the later token callback.
        apns_token: crate::push_register::apns_token().unwrap_or_default(),
        // A debug build's token is issued by the APNs sandbox; release/TestFlight
        // by production. The gateway tracks this per-device to pick the right host.
        apns_env: if cfg!(debug_assertions) {
            ApnsEnv::Sandbox
        } else {
            ApnsEnv::Production
        },
    };

    let (mut client, hello) = PairingClient::start(req).map_err(|e| e.to_string())?;
    send(&mut ws, &hello).await?;

    let PairFrame::HandshakeReply { msg } = recv_timeout(&mut ws, HANDSHAKE_REPLY_TIMEOUT).await?
    else {
        return Err("expected HandshakeReply".into());
    };
    let final_frame = client.on_handshake_reply(&msg).map_err(|e| e.to_string())?;
    send(&mut ws, &final_frame).await?;

    let confirm_code = client
        .confirm_code()
        .ok_or("confirmation code unavailable")?
        .to_string();

    // Hand the live session to a background pump: it keeps reading the WS while
    // the confirm screen is up (so a gateway abort cancels it), and finishes the
    // handshake once `pair_confirm` relays the user's decision.
    let (decision_tx, decision_rx) = oneshot::channel();
    let (outcome_tx, outcome_rx) = oneshot::channel();
    tokio::spawn(run_pair_pump(
        ws,
        client,
        device_id.clone(),
        keypair,
        remote_api_key.unwrap_or_default(),
        on_abort,
        decision_rx,
        outcome_tx,
    ));

    sessions.0.lock().await.insert(
        device_id.clone(),
        PairControl {
            decision_tx,
            outcome_rx,
        },
    );

    Ok(PairChallenge {
        device_id,
        confirm_code,
    })
}

/// Phase 2: relay the user's decision to the pump task (which owns the live WS)
/// and await its outcome. On accept (and once the operator also confirms) the
/// gateway returns the sealed welcome and pairing finalizes; on decline — or if
/// the gateway already aborted — this returns an error.
pub async fn pair_confirm(
    sessions: &PairingSessions,
    device_id: &str,
    accepted: bool,
) -> Result<PairedSummary, String> {
    let control = sessions
        .0
        .lock()
        .await
        .remove(device_id)
        .ok_or("no pending pairing session for this device")?;

    if control.decision_tx.send(accepted).is_err() {
        // The pump already exited — the gateway aborted before the user tapped
        // (the UI gets a `PairAborted` over the `on_abort` channel in that case).
        return Err("pairing session already ended".into());
    }
    match control.outcome_rx.await {
        Ok(result) => result,
        Err(_) => Err("pairing session ended".into()),
    }
}

/// Own the live WS between `pair_begin` and the user's decision. While the
/// confirm screen is up it watches for a gateway `Reject` (or a dropped link) and
/// pushes [`PairAborted`] so the UI can dismiss the screen; once the user decides
/// (over `decision_rx`) it sends the sealed `DeviceConfirm` and finishes,
/// reporting the result over `outcome_tx`.
#[allow(clippy::too_many_arguments)]
async fn run_pair_pump(
    mut ws: Ws,
    mut client: PairingClient,
    device_id: String,
    keypair: StaticKeypair,
    remote_api_key: String,
    on_abort: Channel<PairAborted>,
    mut decision_rx: oneshot::Receiver<bool>,
    outcome_tx: oneshot::Sender<Result<PairedSummary, String>>,
) {
    // Phase A: parked on the confirm screen. Wait for the user's decision, but
    // keep reading the WS so a gateway abort cancels the screen rather than
    // stranding it until the user taps.
    let accepted = loop {
        tokio::select! {
            biased;
            decided = &mut decision_rx => match decided {
                Ok(accepted) => break accepted,
                // Control dropped without a decision (shouldn't happen) — give up.
                Err(_) => return,
            },
            inbound = ws.next() => match inbound {
                Some(Ok(Message::Binary(bytes))) => match pairing::decode(&bytes) {
                    // Operator declined (or the host aborted) before the user tapped.
                    Ok(PairFrame::Reject { reason }) => {
                        let _ = on_abort.send(PairAborted { reason });
                        let _ = outcome_tx.send(Err("pairing was cancelled".into()));
                        return;
                    }
                    // Any other frame here is unexpected this early; keep waiting.
                    Ok(_) | Err(_) => continue,
                },
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                Some(Ok(_)) => continue,
                Some(Err(e)) => {
                    let _ = on_abort.send(PairAborted {
                        reason: format!("connection error: {e}"),
                    });
                    let _ = outcome_tx.send(Err("pairing connection ended".into()));
                    return;
                }
                None => {
                    let _ = on_abort.send(PairAborted {
                        reason: "connection closed".into(),
                    });
                    let _ = outcome_tx.send(Err("pairing connection closed".into()));
                    return;
                }
            },
        }
    };

    // Phase B: the user decided — send it and finish (or tear down on decline).
    let outcome = finish_pair(
        &mut ws,
        &mut client,
        &device_id,
        &keypair,
        &remote_api_key,
        accepted,
    )
    .await;
    let _ = outcome_tx.send(outcome);
}

/// Send the user's decision and complete the handshake. On accept this reads the
/// sealed `GatewayWelcome` (or a late `Reject` if the operator declined in the
/// window after we accepted), persists the push key + durable pairing record, and
/// returns the summary; on decline it tells the gateway and errors (the UI treats
/// the decline path as an error, as before).
async fn finish_pair(
    ws: &mut Ws,
    client: &mut PairingClient,
    device_id: &str,
    keypair: &StaticKeypair,
    remote_api_key: &str,
    accepted: bool,
) -> Result<PairedSummary, String> {
    let confirm = client.confirm(accepted).map_err(|e| e.to_string())?;
    send(ws, &confirm).await?;
    if !accepted {
        // Wait for the gateway to acknowledge the decline (it replies with a
        // Reject once it has processed our DeviceConfirm) before dropping the
        // socket. On the relay path, closing immediately races the relay's
        // forward of our DeviceConfirm — the gateway would never see it and the
        // operator's `device pair` would never learn we cancelled. The ack proves
        // the round-trip; the timeout bounds it if the gateway is already gone.
        let _ = tokio::time::timeout(DECLINE_ACK_TIMEOUT, recv(ws)).await;
        return Err("pairing cancelled".into());
    }

    let paired = match recv(ws).await? {
        PairFrame::Sealed { msg } => client.on_welcome(&msg).map_err(|e| e.to_string())?,
        // The operator declined in the window between our accept and the welcome.
        PairFrame::Reject { reason } => {
            return Err(format!("the operator declined pairing: {reason}"));
        }
        _ => return Err("expected sealed GatewayWelcome".into()),
    };

    // Persist the push key to the shared App Group keychain so the NSE can
    // decrypt lock-screen previews (account `baybo.push-key.<device_id>`, since
    // the gateway stamps `bid = device_id` into every push payload).
    crate::keychain::store_push_key(device_id, &paired.push_key)
        .map_err(|e| format!("persist push key: {e}"))?;

    // Persist the content-session record (auth token, gateway static key,
    // routing candidates, and the app's Noise secret) so the app can reconnect
    // and open a content session after a relaunch.
    let record = PairedRecord {
        device_id: device_id.to_string(),
        auth_token: paired.auth_token.clone(),
        gateway_static_pubkey: paired.gateway_static_pubkey,
        relay_node_id: paired.relay_node_id.clone(),
        relay_url: paired.relay_url.clone(),
        remote_api_key: remote_api_key.to_string(),
        noise_secret: keypair.secret(),
        noise_public: keypair.public(),
    };
    let bytes = serde_json::to_vec(&record).map_err(|e| format!("encode paired record: {e}"))?;
    crate::keychain::store_paired_record(&bytes)
        .map_err(|e| format!("persist paired record: {e}"))?;
    crate::push_register::spawn_register_current_token();

    Ok(PairedSummary::from(&paired))
}

/// The device id of the persisted pairing, if the app is already paired (so a
/// relaunch can skip straight to "connected" instead of the scan form).
pub fn paired_device() -> Option<String> {
    load_paired_record().ok().flatten().map(|r| r.device_id)
}

/// Forget the current pairing (unpair): clear the persisted record and the
/// device's push key from the shared keychain, returning the app to the
/// unpaired state. One app binds one gateway, so there is exactly one record to
/// drop. Idempotent — succeeds even if nothing was stored.
pub fn forget_pairing() -> Result<(), String> {
    // Read the record first to learn the device_id, so its push key
    // (`baybo.push-key.<device_id>`) is cleared too; a missing record is fine.
    if let Some(record) = load_paired_record()? {
        crate::keychain::delete_push_key(&record.device_id)?;
    }
    crate::keychain::delete_paired_record()
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Dial the relay pairing WS. The operator's host leg may not be parked the
/// instant the app scans, so retry briefly on connect failure.
async fn connect_pair(url: &str, remote_api_key: Option<&str>) -> Result<Ws, String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let attempts = 30;
    let mut last = String::new();
    for i in 0..attempts {
        // A Request is consumed by `connect_async` and isn't cheaply cloneable,
        // so rebuild it each attempt. On the relay path the app presents the QR's
        // admission key as `x-remote-api-key`, matching the gateway's host leg.
        let mut req = match url.into_client_request() {
            Ok(r) => r,
            Err(e) => return Err(format!("bad pairing url {url}: {e}")),
        };
        if let Some(key) = remote_api_key {
            match key.parse() {
                Ok(v) => {
                    req.headers_mut()
                        .insert(remote_host_protocol::relay::REMOTE_API_KEY_HEADER, v);
                }
                Err(e) => return Err(format!("bad instance key header: {e}")),
            }
        }
        match connect_async(req).await {
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

/// [`recv`] with a deadline, so a stalled / hostile relay can't pin the app open
/// waiting on a frame that never arrives.
async fn recv_timeout(ws: &mut Ws, timeout: Duration) -> Result<PairFrame, String> {
    match tokio::time::timeout(timeout, recv(ws)).await {
        Ok(result) => result,
        Err(_) => Err("timed out waiting for the gateway".into()),
    }
}

/// Decode the QR's hex `s=` into the 32-byte Noise PSK. Rejects anything that
/// isn't exactly [`KEY_LEN`] bytes — the load-bearing high-entropy invariant
/// (a short secret would be offline-crackable by the relay).
fn decode_secret(s: &str) -> Result<PairingSecret, String> {
    let bytes = hex::decode(s.trim()).map_err(|_| "pairing secret is not valid hex".to_string())?;
    let arr: [u8; KEY_LEN] = bytes
        .try_into()
        .map_err(|_| format!("pairing secret must be {KEY_LEN} bytes"))?;
    Ok(PairingSecret::from_bytes(arr))
}
