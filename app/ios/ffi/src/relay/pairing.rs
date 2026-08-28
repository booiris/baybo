//! The pairing transport: dial the gateway's pair-rendezvous WS and drive the
//! XXpsk0 + mutual-confirm handshake through the local protocol core's
//! `PairingClient`.
//!
//! The handshake pauses for a human decision, so it is split across two FFI
//! calls: [`pair_begin`] runs up to the confirmation step and returns the
//! Bluetooth-style code for the UI to show; [`pair_confirm`] sends the user's
//! decision and finishes. Between the two, the live WS is owned by a background
//! pump task ([`run_pair_pump`]) — keyed by device id in [`PairingSessions`] via
//! a [`PairControl`] handle — so the app keeps *listening* while the confirm
//! screen is up: if the gateway aborts (the operator declined, or the link
//! dropped) before the user taps, the pump calls the [`PairAbortListener`] and
//! the UI dismisses the screen instead of hanging.
//!
//! The crypto + state machine live in the host-tested core; this file is just
//! the WebSocket pump (msgpack `PairFrame`s as binary frames) + the bits the
//! shell owns: loading-or-minting the device's persistent Noise keypair (and the
//! device id derived from it).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use device_proto::aead::KEY_LEN;
use device_proto::delegation;
use device_proto::noise::StaticKeypair;
use device_proto::pairing::{self, PairFrame};
use device_proto::psk_pair::PairingSecret;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, oneshot};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::api::{PairAbortListener, PairTarget};
use crate::core::{PairChallenge, PairedSummary, PairingClient, PairingRequest};

/// How long the decline path waits for the gateway to acknowledge our
/// `DeviceConfirm(false)` before dropping the socket — long enough for the relay
/// round-trip, so the operator's side reliably learns we cancelled.
const DECLINE_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait for the gateway's `HandshakeReply` before giving up. Without
/// it a malicious relay could keep the app pinned open indefinitely on the first
/// reply (a `msg2` that never arrives).
const HANDSHAKE_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the accept path waits for the sealed `GatewayWelcome`. Generous
/// because the gateway-side operator may still need to confirm (human latency),
/// but bounded so a dead relay/gateway can't pin `pair_confirm` — and with it
/// the app's busy flag, which freezes the confirm screen — open forever.
/// Dropping the socket on timeout makes the gateway's side abort cleanly.
const WELCOME_TIMEOUT: Duration = Duration::from_secs(120);

/// The one warn every missed push-delegation step shares (see [`finish_pair`]) —
/// pairing reports success, so this line is the only trace that pushes for the
/// device were never provisioned.
const PUSH_DELEGATION_MISS: &str =
    "pairing succeeded but push delegation was not sent; pushes for this device will not work";

/// In-flight pairing sessions, parked between `pair_begin` and `pair_confirm`
/// (keyed by device id). The map lock is held only to insert/remove; the live WS
/// is owned by the background [`run_pair_pump`] task, reached through this handle.
#[derive(Default)]
pub(crate) struct PairingSessions(Mutex<HashMap<String, PairControl>>);

/// Control handle to a parked session's [`run_pair_pump`] task.
struct PairControl {
    /// Hands the user's Pair/Cancel decision to the pump.
    decision_tx: oneshot::Sender<bool>,
    /// Resolves with the pump's terminal outcome once the user has decided.
    outcome_rx: oneshot::Receiver<Result<PairedSummary, String>>,
}

/// The durable record persisted after pairing — everything the app needs to
/// reconnect and open a content session after a relaunch. Serialized into the
/// keychain (`keychain::store_paired_record`) as JSON; the byte format is the
/// upgrade-continuity contract with already-shipped installs, so field names
/// must not change.
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

/// Whether a scan-to-pair record is persisted AND still decodes — the `relay`
/// arm of the active-binding resolver ([`crate::binding`]).
///
/// The decode is the point. A populated slot whose bytes no longer parse is
/// reported as NOT paired, so the app falls back to the pair/login screen and
/// the user can re-pair over the bad blob. Answering `true` there is the one
/// reply that strands them: [`crate::binding::active_leg`] would route every
/// chat and blob command to the relay leg, each would fail at
/// [`load_paired_record`], and the app would sit claiming to be paired with no
/// error to act on and no way out. It also kept `has_pairing` and
/// [`paired_device`] — which has always answered `None` on a decode error —
/// disagreeing about the same bytes.
///
/// A keychain READ error propagates rather than reading as unpaired, and that
/// is real rather than decorative: `keychain::classify_read` answers absence for
/// `errSecItemNotFound` and only that, so a transient failure —
/// `errSecInteractionNotAllowed` before the first unlock since boot, which these
/// `…AfterFirstUnlockThisDeviceOnly` items make reachable on a background wake —
/// arrives here as `Err` and does NOT invite a re-pair over a good record.
pub(crate) fn has_pairing() -> Result<bool, String> {
    Ok(crate::keychain::read_paired_record()?.is_some_and(|bytes| record_decodes(&bytes)))
}

/// The decode half of [`has_pairing`], split out because it is the only half
/// testable off-device — the host keychain stub always reads empty.
fn record_decodes(bytes: &[u8]) -> bool {
    match serde_json::from_slice::<PairedRecord>(bytes) {
        Ok(_) => true,
        Err(e) => {
            log::warn!("paired-gateway record present but undecodable, treating as unpaired: {e}");
            false
        }
    }
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
/// persisted on first use. On a host build (no on-device keychain) the read is
/// always empty, so each run mints an ephemeral key — fine off-device.
fn load_or_create_device_identity() -> Result<StaticKeypair, String> {
    if let Some((secret, public)) = crate::keychain::read_device_identity()? {
        return Ok(StaticKeypair::from_parts(public, secret));
    }
    let keypair = StaticKeypair::generate().map_err(|e| e.to_string())?;
    crate::keychain::store_device_identity(&keypair.secret(), &keypair.public())?;
    Ok(keypair)
}

/// Dial the target's endpoint, run XXpsk0 through the `DeviceHello` step, and
/// return the confirmation code for the user to compare against the operator's
/// terminal. Hands the live session to a background [`run_pair_pump`] task that
/// watches for a gateway abort (calling `on_abort`) and finishes the handshake
/// once the user decides via [`pair_confirm`].
pub(crate) async fn pair_begin(
    sessions: &PairingSessions,
    target: PairTarget,
    on_abort: Arc<dyn PairAbortListener>,
) -> Result<PairChallenge, String> {
    let PairTarget {
        endpoint,
        rendezvous_id,
        secret,
        remote_api_key,
    } = target;
    let base = endpoint.trim_end_matches('/');
    // Pairing is relay-only: join the rendezvous keyed by the public rendezvous
    // id (`/pair/join/{rendezvous_id}`); the operator's `baybo device pair` hosts
    // the other leg on the relay.
    let url = remote_host_protocol::relay::pair_join_url(base, &rendezvous_id);
    let mut ws = connect_pair(&url, remote_api_key.as_deref()).await?;

    // Decode the QR secret (the Noise PSK). It must be exactly 32 bytes — a
    // short/typeable secret is forbidden (offline-crackable by the relay).
    let secret = decode_secret(&secret)?;

    // The app's long-term Noise identity, loaded from (or minted into) the
    // keychain so the derived device_id stays stable across re-pairings and
    // launches, and content sessions reuse the same static across relaunches.
    let keypair = load_or_create_device_identity()?;
    // The device's Ed25519 push-delegation identity; its public half IS the
    // device_id, so C can self-certify the push binding. Separate from the X25519
    // Noise static above (which authenticates content sessions). Shared with the
    // direct push-register path so a phone keeps one stable push identity.
    let sign_key = crate::keychain::load_or_create_device_sign_key()?;
    let device_id = delegation::device_id_for(&sign_key.verifying_key());

    let req = PairingRequest {
        rendezvous_id: rendezvous_id.clone(),
        secret,
        // Bound into the handshake prologue (anti-cross-binding); must match the
        // gateway's relay URL — it does, both being the QR `h=`.
        endpoint: endpoint.clone(),
        device_id: device_id.clone(),
        // The app static rides the handshake as an XX token; the gateway learns
        // the public half in-band.
        static_secret: keypair.secret(),
    };

    let (mut client, hello) = PairingClient::start(req).map_err(|e| e.to_string())?;
    send(&mut ws, &hello).await?;

    // Bounded wait, so a stalled / hostile relay can't pin the app open on a
    // `msg2` that never arrives.
    let reply = match tokio::time::timeout(HANDSHAKE_REPLY_TIMEOUT, recv(&mut ws)).await {
        Ok(Ok(frame)) => frame,
        Ok(Err(e)) => {
            log::warn!("pairing: failed reading HandshakeReply (rendezvous={rendezvous_id}): {e}");
            return Err(e);
        }
        Err(_) => {
            log::warn!(
                "pairing: no HandshakeReply from gateway within {}s (rendezvous={rendezvous_id}; operator leg absent or relay dropped the forward)",
                HANDSHAKE_REPLY_TIMEOUT.as_secs()
            );
            return Err("timed out waiting for the gateway".into());
        }
    };
    let PairFrame::HandshakeReply { msg } = reply else {
        log::warn!(
            "pairing: expected HandshakeReply, got another frame (rendezvous={rendezvous_id})"
        );
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
        sign_key,
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
pub(crate) async fn pair_confirm(
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
        // (the UI gets an `on_abort` call in that case).
        return Err("pairing session already ended".into());
    }
    match control.outcome_rx.await {
        Ok(result) => result,
        Err(_) => Err("pairing session ended".into()),
    }
}

/// Own the live WS between `pair_begin` and the user's decision. While the
/// confirm screen is up it watches for a gateway `Reject` (or a dropped link) and
/// calls `on_abort` so the UI can dismiss the screen; once the user decides
/// (over `decision_rx`) it sends the sealed `DeviceConfirm` and finishes,
/// reporting the result over `outcome_tx`.
#[allow(clippy::too_many_arguments)]
async fn run_pair_pump(
    mut ws: Ws,
    mut client: PairingClient,
    device_id: String,
    keypair: StaticKeypair,
    sign_key: delegation::SigningKey,
    remote_api_key: String,
    on_abort: Arc<dyn PairAbortListener>,
    mut decision_rx: oneshot::Receiver<bool>,
    outcome_tx: oneshot::Sender<Result<PairedSummary, String>>,
) {
    // Phase A: parked on the confirm screen. Wait for the user's decision, but
    // keep reading the WS so a gateway abort cancels the screen rather than
    // stranding it until the user taps.
    // Once-per-pump latches: the relay leg is unauthenticated beyond admission,
    // so an unintelligible stream must not produce a log line per frame.
    let mut logged_unexpected = false;
    let mut logged_undecodable = false;
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
                        on_abort.on_abort(reason);
                        let _ = outcome_tx.send(Err("pairing was cancelled".into()));
                        return;
                    }
                    // Any other frame here is unexpected this early; keep waiting.
                    Ok(_) => {
                        if !logged_unexpected {
                            logged_unexpected = true;
                            log::debug!(
                                "pairing: ignoring unexpected frame(s) while awaiting user decision (device={device_id}; logged once)"
                            );
                        }
                        continue;
                    }
                    Err(e) => {
                        if !logged_undecodable {
                            logged_undecodable = true;
                            log::debug!(
                                "pairing: ignoring undecodable frame(s) while awaiting user decision (device={device_id}, {} bytes; logged once): {e}",
                                bytes.len()
                            );
                        }
                        continue;
                    }
                },
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                Some(Ok(_)) => continue,
                Some(Err(e)) => {
                    on_abort.on_abort(format!("connection error: {e}"));
                    let _ = outcome_tx.send(Err("pairing connection ended".into()));
                    return;
                }
                None => {
                    on_abort.on_abort("connection closed".into());
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
        &sign_key,
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
    sign_key: &delegation::SigningKey,
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

    let welcome = tokio::time::timeout(WELCOME_TIMEOUT, recv(ws))
        .await
        .map_err(|_| "pairing timed out waiting for the gateway's welcome".to_string())??;
    let paired = match welcome {
        PairFrame::Sealed { msg } => client.on_welcome(&msg).map_err(|e| e.to_string())?,
        // The operator declined in the window between our accept and the welcome.
        PairFrame::Reject { reason } => {
            return Err(format!("the operator declined pairing: {reason}"));
        }
        _ => return Err("expected sealed GatewayWelcome".into()),
    };

    // 6th message: authorize the gateway's push key to manage our push binding at
    // C, signed under our Ed25519 device identity (whose public half is the
    // device_id). Best-effort — if the gateway advertised no push key (relay off)
    // or the send fails, the pairing still stands; only push is affected, so each
    // miss is a warn rather than a pairing failure.
    match paired.gateway_push_pubkey {
        None => {
            log::warn!("{PUSH_DELEGATION_MISS} (device={device_id} reason=no_gateway_push_key)")
        }
        Some(gw_push) => match delegation::verifying_key_from_bytes(&gw_push) {
            Err(e) => {
                log::warn!("{PUSH_DELEGATION_MISS} (device={device_id} reason=bad_push_key): {e}")
            }
            Ok(gw_push_key) => {
                let sig = delegation::sign_delegation(sign_key, &gw_push_key);
                match client.seal_delegation(sig.to_bytes().to_vec()) {
                    Err(e) => log::warn!(
                        "{PUSH_DELEGATION_MISS} (device={device_id} reason=seal_failed): {e}"
                    ),
                    Ok(frame) => {
                        if let Err(e) = send(ws, &frame).await {
                            log::warn!(
                                "{PUSH_DELEGATION_MISS} (device={device_id} reason=send_failed): {e}"
                            );
                        }
                    }
                }
            }
        },
    }

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
    // Record which bind happened last BEFORE committing the pairing record, so the
    // resolver breaks a transient both-present tie toward relay — the binding just
    // made. Ordered first on purpose: a failed marker write must not leave the record
    // committed under a stale marker (which would resolve the tie to the superseded
    // leg). A marker with the record absent is harmless — active_leg only consults it
    // when both credentials exist.
    crate::keychain::store_active_binding(crate::binding::RELAY_MARKER)
        .map_err(|e| format!("persist active binding: {e}"))?;
    crate::keychain::store_paired_record(&bytes)
        .map_err(|e| format!("persist paired record: {e}"))?;
    // A fresh scan overwrites an existing pairing WITHOUT going through
    // `forget_pairing`, and re-pairing the SAME gateway keeps every field of the
    // leg pool's `BindingKey` — the Noise static and the signing key that
    // `device_id` derives from are never deleted, and the gateway's static key and
    // relay node id do not move. So a leg parked before the re-pair would pass
    // every reuse gate while carrying the gateway's snapshot of the auth token it
    // has just rotated away: a 401, and (because a 401 is a perfectly well-formed
    // response) one that gets re-parked with a refreshed TTL. Drop them.
    super::leg_pool::pool().invalidate();
    // One app binds one Baybo: a fresh scan-pairing supersedes any direct-login
    // credentials so the two binding modes can't both linger. Best-effort — the
    // marker above keeps resolution correct even if this leaves the direct creds.
    let _ = crate::keychain::delete_direct_credentials();

    Ok(PairedSummary::from(&paired))
}

/// The device id of the persisted pairing, if the app is already paired (so a
/// relaunch can skip straight to "connected" instead of the scan form).
pub(crate) fn paired_device() -> Option<String> {
    load_paired_record().ok().flatten().map(|r| r.device_id)
}

/// Forget the current pairing (unpair): clear the persisted record and the
/// device's push key from the shared keychain, returning the app to the
/// unpaired state. One app binds one gateway, so there is exactly one record to
/// drop. Idempotent — succeeds even if nothing was stored.
pub(crate) fn forget_pairing() -> Result<(), String> {
    // Every warm API leg belongs to the binding we are dropping. Each one holds a
    // gateway-side `AuthenticatedDevice` snapshotted at ITS handshake, so a leg
    // that outlives the credentials answers with a token the gateway has already
    // rotated away. Do this HERE rather than in each caller: `logout`, a direct
    // login that supersedes a pairing, and a re-pair all reach this function, and
    // none of them should have to remember.
    super::leg_pool::pool().invalidate();
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
                        .insert(remote_host_protocol::REMOTE_API_KEY_HEADER, v);
                }
                Err(e) => return Err(format!("bad instance key header: {e}")),
            }
        }
        match connect_async(req).await {
            Ok((ws, _)) => {
                if i > 0 {
                    log::info!("pairing rendezvous connected after {i} failed attempts ({url})");
                }
                return Ok(ws);
            }
            Err(e) => {
                last = format!("connect {url}: {e}");
                if i + 1 < attempts {
                    log::debug!(
                        "pairing rendezvous dial failed; retrying (attempt {}/{attempts}, url={url}): {}",
                        i + 1,
                        super::dial::ws_error_detail(&e)
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }
    }
    log::warn!("pairing dial failed after {attempts} attempts: {last}");
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

/// The keychain byte format of [`PairedRecord`] is the upgrade-continuity contract
/// with already-shipped installs: a paired user who upgrades reads THIS blob
/// back. A renamed field, a re-typed key, or a dropped `#[serde(default)]`
/// deserializes to `None`/an error and the app silently forgets its gateway
/// binding — no error the user can act on, no way back but a re-pair. So the
/// assertions below are on the literal JSON TEXT, not on a round-trip (which
/// passes happily with every field renamed).
#[cfg(test)]
mod tests {
    use super::*;

    /// The exact bytes `finish_pair` writes to `baybo.paired-gateway`. Every field
    /// name here is frozen; the two 32-byte keys are JSON ARRAYS of numbers.
    const GOLDEN_RECORD_JSON: &str = r#"{"device_id":"device-abc","auth_token":"tok-1","gateway_static_pubkey":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"relay_node_id":"node-1","relay_url":"wss://relay.example","remote_api_key":"key-1","noise_secret":[2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2],"noise_public":[3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3,3]}"#;

    /// A record missing all three `#[serde(default)]` fields. **No shipped build
    /// ever wrote this shape** — `relay_url`, `remote_api_key`, and
    /// `noise_public` have carried their defaults since the first commit of the
    /// first iOS app, and nothing serializes the record without them. So this
    /// fixture does not stand in for any real install; the defaults are a
    /// failure-mode choice and this pins it.
    ///
    /// The choice: `has_pairing` treats an undecodable record as UNPAIRED, which
    /// costs the user a re-pair. Defaulting the three fields that can carry a
    /// sane empty value keeps a record that is merely short one of them loading
    /// and working instead — `relay_url` and `remote_api_key` are legitimately
    /// empty on a relay-off pairing anyway, and `dial` already guards on both.
    /// The fields with no default stay required, so a record missing something
    /// with no sane empty value still fails loudly rather than half-loading.
    const LEGACY_RECORD_JSON: &str = r#"{"device_id":"device-legacy","auth_token":"tok-old","gateway_static_pubkey":[9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9,9],"relay_node_id":"node-old","noise_secret":[8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8,8]}"#;

    fn golden_record() -> PairedRecord {
        PairedRecord {
            device_id: "device-abc".to_string(),
            auth_token: "tok-1".to_string(),
            gateway_static_pubkey: [1u8; KEY_LEN],
            relay_node_id: "node-1".to_string(),
            relay_url: "wss://relay.example".to_string(),
            remote_api_key: "key-1".to_string(),
            noise_secret: [2u8; KEY_LEN],
            noise_public: [3u8; KEY_LEN],
        }
    }

    #[test]
    fn the_serialized_record_is_byte_for_byte_the_frozen_format() {
        let json = serde_json::to_string(&golden_record()).expect("serialize");
        assert_eq!(json, GOLDEN_RECORD_JSON);
    }

    /// Someone WILL want to "clean up" the 32-number arrays into a hex string.
    /// That reads back as a type error on every existing install.
    #[test]
    fn the_key_fields_serialize_as_arrays_of_32_numbers_never_hex() {
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&golden_record()).expect("serialize"))
                .expect("parse");
        for field in ["gateway_static_pubkey", "noise_secret", "noise_public"] {
            let value = &json[field];
            let array = value
                .as_array()
                .unwrap_or_else(|| panic!("{field} must be a JSON array, got {value}"));
            assert_eq!(array.len(), KEY_LEN, "{field} must carry {KEY_LEN} bytes");
            assert!(
                array.iter().all(|byte| byte.is_u64()),
                "{field} must be numbers, not a hex string"
            );
        }
    }

    #[test]
    fn the_field_names_are_frozen() {
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&golden_record()).expect("serialize"))
                .expect("parse");
        let mut keys: Vec<&str> = json
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "auth_token",
                "device_id",
                "gateway_static_pubkey",
                "noise_public",
                "noise_secret",
                "relay_node_id",
                "relay_url",
                "remote_api_key",
            ]
        );
    }

    #[test]
    fn a_record_short_a_defaulted_field_degrades_instead_of_failing() {
        let record: PairedRecord = serde_json::from_str(LEGACY_RECORD_JSON).expect("decode legacy");
        assert_eq!(record.device_id, "device-legacy");
        assert_eq!(record.auth_token, "tok-old");
        assert_eq!(record.gateway_static_pubkey, [9u8; KEY_LEN]);
        assert_eq!(record.relay_node_id, "node-old");
        assert_eq!(record.noise_secret, [8u8; KEY_LEN]);
        assert_eq!(record.relay_url, "");
        assert_eq!(record.remote_api_key, "");
        assert_eq!(record.noise_public, [0u8; KEY_LEN]);
    }

    /// The other half of the contract: the fields with no `#[serde(default)]` are
    /// REQUIRED, so a blob missing one is a loud decode error rather than a record
    /// that silently reconnects with an empty auth token.
    #[test]
    fn a_record_missing_a_required_field_fails_to_decode() {
        let missing_token = LEGACY_RECORD_JSON.replace(r#""auth_token":"tok-old","#, "");
        assert!(serde_json::from_str::<PairedRecord>(&missing_token).is_err());
    }

    #[test]
    fn a_record_that_decodes_counts_as_paired() {
        assert!(record_decodes(GOLDEN_RECORD_JSON.as_bytes()));
        // Short the defaulted fields and it must still count — that is what the
        // defaults buy, and reading it as unpaired would cost a needless re-pair.
        assert!(record_decodes(LEGACY_RECORD_JSON.as_bytes()));
    }

    /// The stranding case: a populated slot whose bytes no longer parse must read
    /// as UNPAIRED, so the app offers the pairing screen instead of routing every
    /// command to a relay leg that cannot construct. Answering `true` here left
    /// `has_pairing` and `paired_device` disagreeing about the same bytes.
    #[test]
    fn a_record_that_no_longer_decodes_reads_as_unpaired() {
        assert!(!record_decodes(b""));
        assert!(!record_decodes(br#"{"device_id":"truncated-mid"#));
        assert!(!record_decodes(b"not json at all"));
        let missing_token = LEGACY_RECORD_JSON.replace(r#""auth_token":"tok-old","#, "");
        assert!(!record_decodes(missing_token.as_bytes()));
    }

    /// A newer iOS build may add a field this build doesn't know; reading the
    /// blob must not fail on it.
    #[test]
    fn an_unknown_field_from_a_newer_build_is_ignored() {
        let with_extra = GOLDEN_RECORD_JSON.replace(
            r#"{"device_id""#,
            r#"{"future_field":"whatever","device_id""#,
        );
        let record: PairedRecord = serde_json::from_str(&with_extra).expect("decode");
        assert_eq!(record.device_id, "device-abc");
    }

    #[test]
    fn the_golden_blob_re_serializes_to_itself() {
        let record: PairedRecord = serde_json::from_str(GOLDEN_RECORD_JSON).expect("decode");
        assert_eq!(
            serde_json::to_string(&record).expect("serialize"),
            GOLDEN_RECORD_JSON
        );
    }

    /// Dropping the binding has to drop the warm API legs with it.
    ///
    /// The gateway resolves a leg's `AuthenticatedDevice` ONCE, at its handshake, so
    /// a leg that outlives the credentials keeps presenting a token the gateway has
    /// already rotated away. And re-pairing the SAME gateway changes NOTHING the leg
    /// pool keys on — the Noise static and the signing key `device_id` derives from
    /// are never deleted, and the gateway's static key and relay node id do not move
    /// — so without this the pool would hand a freshly re-paired app a leg that 401s
    /// every call.
    #[test]
    fn forgetting_a_pairing_drops_every_warm_api_leg() {
        let before = super::super::leg_pool::pool().epoch();

        // The keychain wipe may fail on a host build; the invalidation runs first
        // precisely because it must not depend on it.
        let _ = forget_pairing();

        assert!(
            super::super::leg_pool::pool().epoch() > before,
            "a leg must not outlive the credentials it was authenticated with"
        );
    }
}
