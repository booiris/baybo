//! The XXpsk0 device-pairing handshake ([`drive`]) with a Bluetooth-style mutual
//! confirm, plus the in-process [`PairingHostDeps`] it runs against.
//!
//! Pairing is driven entirely by one interactive `baybo device pair`: it hosts an
//! outbound relay leg (see [`super::relay_pair::host_pairing_leg`]) and runs this
//! handshake over it. There is no daemon-served pairing route — the gateway only
//! serves *content* for already-paired devices.
//!
//! Pairing happens *before* any auth token exists, so it carries no channel-token
//! middleware. Its security comes from the QR **secret**, carried out of band and
//! used as the Noise **PSK**: the relay (**C**) sees only the public
//! `rendezvous_id` and opaque Noise frames, so — lacking the PSK — it cannot
//! complete the handshake with either side and is reduced from MITM to
//! denial-of-service. On top of that, a two-sided confirm authorizes the pairing:
//! the phone user and the operator each approve a confirmation code derived from
//! the Noise handshake hash `h` before any token activates. The handshake (see
//! [`device_proto::pairing::PairFrame`]):
//!
//! 1. P → A `Hello { rendezvous_id, msg }` — A claims the slot, loads the secret,
//!    builds the XXpsk0 responder, and reads the app's `msg1`.
//! 2. A → P `HandshakeReply { msg }` — A's `msg2` (its static rides as an XX token).
//! 3. P → A `HandshakeFinal { msg }` — the app's `msg3`, whose Noise payload is
//!    the `DeviceHello` body; both ends are now in transport mode and share `h`.
//! 4. P → A `Sealed(DeviceConfirm)` — the phone user's decision; A also waits for
//!    the operator's `y` on the live `device pair`.
//! 5. A → P `Sealed(GatewayWelcome)` — routing + active `auth_token`.
//!
//! On mutual confirm an **approved** device row exists (its `auth_token` active,
//! its `device_pubkey` the app static learned in-band) and the per-device push
//! key (HKDF of `h`) is stored in A's `SecretVault`. C never sees any of this.

use std::sync::Arc;
use std::time::Duration;

use device_proto::delegation;
use device_proto::kdf::{derive_confirm_code, derive_push_key};
use device_proto::pairing::{
    self, DeviceConfirm, DeviceDelegation, DeviceHello, GatewayWelcome, PairFrame,
};
use device_proto::psk_pair::{PskHandshake, PskTransport, build_prologue};

use baybo_pairing::DevicePairingService;
use baybo_security::SecretVault;

use crate::device::load_or_create_static_keypair;

/// The dependencies the pairing handshake ([`drive`]) needs, built by the
/// operator's `baybo device pair` from CLI deps. Carried explicitly so [`drive`]
/// can run over the relay host leg (see [`super::relay_pair::host_pairing_leg`])
/// without a full gateway.
#[derive(Clone)]
pub struct PairingHostDeps {
    pub device_pairing: Arc<DevicePairingService>,
    pub secret_vault: Arc<SecretVault>,
    /// Base WS URL of the relay, advertised to the app in
    /// `GatewayWelcome.relay_url` and gating the `relay_node_id`; empty = no relay.
    /// It is also bound into the handshake prologue, so the app's QR `h=` and this
    /// value must agree (anti-cross-binding: the relay can't wire the app to a
    /// gateway on a different endpoint).
    pub relay_url: String,
    /// HTTP base for push `/register` + `/notify`. Kept separate from the relay
    /// proxy because the public services may use different hosts.
    pub push_url: String,
    /// The relay admission key (`x-remote-api-key`) the pairing legs present.
    /// Persisted on the device row so the gateway's later relay control and data
    /// legs reuse it without a config block. Push does not carry this key.
    pub remote_api_key: String,
}

/// Per-step receive timeout — a stalled peer must not pin a connection.
const PAIR_STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the confirm step waits on a human (the phone user's tap and the
/// operator's `y`) — longer than a protocol step since people are deciding.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(120);

/// Poll cadence for the operator's confirm decision on the shared slot.
const CONFIRM_POLL_INTERVAL: Duration = Duration::from_millis(400);

/// How long A waits, after the welcome, for the device's push delegation. Bounded
/// short: its absence only disables push for the device (relay/content are
/// unaffected), so a misbehaving or older app must not pin the leg for long.
const DELEGATION_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) async fn drive<T: PairTransport + ?Sized>(
    transport: &mut T,
    state: &PairingHostDeps,
) -> Result<(), String> {
    // 1. Hello — the public rendezvous id + the app's first handshake message.
    let PairFrame::Hello { rendezvous_id, msg } = transport.recv_frame(PAIR_STEP_TIMEOUT).await?
    else {
        return Err("expected Hello frame".into());
    };

    // 2. Authorize against the live slot (which holds the QR secret), then build
    //    the XXpsk0 responder. An unknown / expired rendezvous is refused before
    //    any crypto runs; a wrong PSK fails the handshake below (fail closed).
    let slot = state
        .device_pairing
        .claim_slot(&rendezvous_id)
        .await
        .map_err(|e| format!("claim slot: {e}"))?
        .ok_or_else(|| "unknown or expired rendezvous id".to_string())?;

    // A's static Noise identity (loaded/created from the vault) — its secret is
    // the responder's static, exchanged in-band as an XX token.
    let static_key = load_or_create_static_keypair(&state.secret_vault)
        .await
        .map_err(|e| format!("static key: {e}"))?;

    let prologue = build_prologue(&rendezvous_id, &state.relay_url);
    let mut handshake =
        PskHandshake::start_responder(&static_key.secret(), &slot.secret, &prologue)
            .map_err(|e| format!("build responder: {e}"))?;
    handshake
        .read_handshake(&msg)
        .map_err(|e| format!("read msg1: {e}"))?;

    // 3. Hand the app our msg2; it learns A's static (as an XX token) from it.
    let msg2 = handshake
        .write_handshake(&[])
        .map_err(|e| format!("write msg2: {e}"))?;
    transport
        .send_frame(&PairFrame::HandshakeReply { msg: msg2 })
        .await?;

    // 4. msg3 — the app's static (XX token) + the DeviceHello body as the Noise
    //    payload. A PSK mismatch / tampered frame aborts here.
    let PairFrame::HandshakeFinal { msg } = transport.recv_frame(PAIR_STEP_TIMEOUT).await? else {
        return Err("expected HandshakeFinal".into());
    };
    let hello_body = handshake
        .read_handshake(&msg)
        .map_err(|e| format!("read msg3: {e}"))?;
    let mut noise = handshake
        .into_transport()
        .map_err(|e| format!("enter transport mode: {e}"))?;
    let hello: DeviceHello =
        pairing::decode(&hello_body).map_err(|e| format!("decode DeviceHello: {e}"))?;
    // The app's authenticated static, learned in-band — the identity the durable
    // row pins and the content channel later authenticates against.
    let app_static = *noise.remote_static();

    // 5. Mutual confirm. The confirm code is the channel binding: HKDF of the
    //    Noise handshake hash `h`, which commits to both statics and the whole
    //    live transcript, so it is not grindable or precomputable. Publish it
    //    onto the slot so the operator's live `device pair` shows it, then require
    //    BOTH the phone user and the operator to approve before any token activates.
    let confirm_code =
        derive_confirm_code(noise.handshake_hash()).map_err(|e| format!("confirm code: {e}"))?;
    state
        .device_pairing
        .publish_confirm(&slot.rendezvous_id, &confirm_code, &hello.device_id)
        .await
        .map_err(|e| format!("publish confirm: {e}"))?;

    // 5a/5b. Mutual confirm, awaited *concurrently*: the phone user's decision
    //   arrives as a sealed DeviceConfirm over the transport, while the operator's
    //   decision is polled from the shared slot (their live `device pair` writes
    //   it). Both must be "accept" — but the moment either side declines (or the
    //   phone drops) we abort so the *other* side isn't left hanging: the phone
    //   learns via the Reject `run_pairing`/`host_leg_once` sends on our `Err`, and
    //   the operator learns via the `device_decision` we record on the slot.
    {
        let deadline = tokio::time::Instant::now() + CONFIRM_TIMEOUT;
        let phone_confirm = transport.recv_frame(CONFIRM_TIMEOUT);
        tokio::pin!(phone_confirm);
        let mut phone_ok = false;
        let mut operator_ok = false;
        while !(phone_ok && operator_ok) {
            tokio::select! {
                biased;
                // The phone user's sealed DeviceConfirm — awaited only until it lands.
                frame = &mut phone_confirm, if !phone_ok => match phone_decision(frame, &mut noise) {
                    PhoneDecision::Accepted => phone_ok = true,
                    PhoneDecision::Declined => {
                        record_device_decline(state, &slot.rendezvous_id).await;
                        return Err("the device declined pairing".into());
                    }
                    PhoneDecision::Aborted(reason) => {
                        record_device_decline(state, &slot.rendezvous_id).await;
                        return Err(reason);
                    }
                },
                // The operator's decision on the shared slot.
                _ = tokio::time::sleep(CONFIRM_POLL_INTERVAL), if !operator_ok => {
                    let decision = state
                        .device_pairing
                        .claim_slot(&slot.rendezvous_id)
                        .await
                        .map_err(|e| format!("poll operator decision: {e}"))?
                        .ok_or_else(|| {
                            "pairing slot expired before the operator decided".to_string()
                        })?
                        .operator_decision;
                    match decision {
                        Some(true) => operator_ok = true,
                        Some(false) => return Err("the operator declined pairing".into()),
                        None if tokio::time::Instant::now() >= deadline => {
                            return Err("timed out waiting for the operator to confirm".into());
                        }
                        None => {}
                    }
                }
            }
        }
    }

    let push_key =
        derive_push_key(noise.handshake_hash()).map_err(|e| format!("derive push key: {e}"))?;
    let push_key_name = crate::push::device_push_key_secret_name(&hello.device_id);
    let (relay_node_id, relay_url) = if state.relay_url.is_empty() {
        (String::new(), String::new())
    } else {
        let node_id = crate::relay::load_or_create_relay_node_id(&state.secret_vault)
            .await
            .map_err(|e| format!("relay node id: {e}"))?;
        (node_id, state.relay_url.clone())
    };

    // A's gateway push-signing key (Ed25519, vault-persisted, stable across
    // restarts). Its public half rides the welcome so the device can sign a
    // delegation authorizing it; its secret signs every later /register and
    // /notify so C can reject an unauthorized caller without a shared admission
    // credential on the push path.
    let push_signing_key = crate::push::load_or_create_push_signing_key(&state.secret_vault)
        .await
        .map_err(|e| format!("push signing key: {e}"))?;
    let gateway_push_pubkey = push_signing_key.verifying_key().to_bytes().to_vec();

    // 6. Stage: write a non-authenticating provisioning row (pinning the app
    //    static learned in-band) + consume the slot (zeroizing the secret it
    //    held). The token becomes active only after the welcome frame has been
    //    sent successfully.
    let staged = state
        .device_pairing
        .stage(
            &slot,
            &hello.device_id,
            app_static.to_vec(),
            &state.relay_url,
            &state.push_url,
            &state.remote_api_key,
        )
        .await
        .map_err(|e| format!("stage pairing: {e}"))?;

    // 6a. Persist the per-device push key (HKDF of `h`) in A's vault, keyed by
    //     device_id (the NSE selects it by `bid` at push time).
    if let Err(e) = state
        .secret_vault
        .store_secret(&push_key_name, &push_key)
        .await
    {
        revoke_staged_device(state, &hello.device_id).await;
        return Err(format!("store push key: {e}"));
    }
    // 7. Sealed GatewayWelcome — the routing the app reaches A by (the gateway's
    //    stable relay_node_id + the relay's base URL when a relay is configured)
    //    + the active auth_token. A's static is not carried: the app already
    //    learned it as an XX token during the handshake.
    let welcome = GatewayWelcome {
        relay_node_id,
        relay_url,
        rendezvous_id: slot.rendezvous_id.clone(),
        // The plaintext, straight from `stage` — the durable row keeps only
        // its digest, so this is the only moment it can be sent.
        auth_token: staged.auth_token.clone(),
        gateway_push_pubkey,
    };
    let sealed = noise
        .write(&pairing::encode(&welcome).map_err(|e| format!("encode welcome: {e}"))?)
        .map_err(|e| format!("seal welcome: {e}"))?;
    if let Err(e) = transport
        .send_frame(&PairFrame::Sealed { msg: sealed })
        .await
    {
        revoke_staged_device(state, &hello.device_id).await;
        return Err(e);
    }

    if let Err(e) = state
        .device_pairing
        .approve_staged(&staged.row.device_id)
        .await
    {
        revoke_staged_device(state, &hello.device_id).await;
        return Err(format!("approve pairing: {e}"));
    }

    // 8. DeviceDelegation — the device authorizes our push key to manage its push
    //    binding at C. Best-effort and after approval: a missing/invalid
    //    delegation only disables push for this device (relay/content still work),
    //    so it must neither delay approval nor fail the pair.
    let gateway_push_pub = push_signing_key.verifying_key();
    match read_device_delegation(transport, &mut noise, &hello.device_id, &gateway_push_pub).await {
        Ok(sig) => {
            if let Err(e) = state
                .secret_vault
                .store_secret(
                    &crate::push::device_push_delegation_secret_name(&hello.device_id),
                    &sig,
                )
                .await
            {
                tracing::warn!(
                    device = %super::short_hash(&hello.device_id),
                    error = %e,
                    "failed to store push delegation; push disabled for this device",
                );
            }
        }
        Err(e) => tracing::warn!(
            device = %super::short_hash(&hello.device_id),
            error = %e,
            "no valid push delegation; push disabled for this device",
        ),
    }

    tracing::info!(
        device = %super::short_hash(&hello.device_id),
        "device paired",
    );
    Ok(())
}

/// Receive the device's sealed [`DeviceDelegation`] (the 6th pairing message) and
/// verify it authorizes our push key. The signature must be by the device
/// identity key whose public half is encoded in `device_id` (so the binding
/// self-certifies at C). Returns the 64-byte delegation signature to persist.
async fn read_device_delegation<T: PairTransport + ?Sized>(
    transport: &mut T,
    noise: &mut PskTransport,
    device_id: &str,
    gateway_push_pub: &delegation::VerifyingKey,
) -> Result<Vec<u8>, String> {
    let PairFrame::Sealed { msg } = transport.recv_frame(DELEGATION_TIMEOUT).await? else {
        return Err("expected sealed DeviceDelegation".into());
    };
    let plaintext = noise
        .read(&msg)
        .map_err(|e| format!("open DeviceDelegation: {e}"))?;
    let deleg: DeviceDelegation =
        pairing::decode(&plaintext).map_err(|e| format!("decode DeviceDelegation: {e}"))?;
    let device_pub = delegation::device_pubkey_from_id(device_id)
        .map_err(|e| format!("device_id is not an Ed25519 identity: {e}"))?;
    let sig = delegation::signature_from_bytes(&deleg.delegation)
        .map_err(|e| format!("malformed delegation signature: {e}"))?;
    if !delegation::verify_delegation(&device_pub, gateway_push_pub, &sig) {
        return Err("device delegation failed to verify".into());
    }
    Ok(deleg.delegation)
}

async fn revoke_staged_device(state: &PairingHostDeps, device_id: &str) {
    if let Err(e) = state.device_pairing.revoke(device_id).await {
        tracing::debug!(error = %e, "device pair: failed to revoke staged device");
    }
}

/// Whether the phone user's sealed [`DeviceConfirm`] (or its absence) lets the
/// handshake continue. Distinguishes an explicit decline from a transport-level
/// abort (the app dropped, or a malformed/unexpected frame) so the caller can
/// surface the right reason — though both record a device-side decline.
enum PhoneDecision {
    Accepted,
    Declined,
    Aborted(String),
}

/// Open the phone's confirm frame (a Noise transport message) and classify the
/// decision. `frame` is the raw receive result, so a transport error (timeout /
/// closed socket) maps to [`PhoneDecision::Aborted`] rather than being lost.
fn phone_decision(frame: Result<PairFrame, String>, noise: &mut PskTransport) -> PhoneDecision {
    let frame = match frame {
        Ok(frame) => frame,
        Err(reason) => return PhoneDecision::Aborted(reason),
    };
    let PairFrame::Sealed { msg } = frame else {
        return PhoneDecision::Aborted("expected sealed DeviceConfirm".into());
    };
    let plaintext = match noise.read(&msg) {
        Ok(pt) => pt,
        Err(e) => return PhoneDecision::Aborted(format!("open DeviceConfirm: {e}")),
    };
    match pairing::decode::<DeviceConfirm>(&plaintext) {
        Ok(confirm) if confirm.accepted => PhoneDecision::Accepted,
        Ok(_) => PhoneDecision::Declined,
        Err(e) => PhoneDecision::Aborted(format!("decode DeviceConfirm: {e}")),
    }
}

/// Best-effort: mark the slot so the operator's live `device pair` learns the
/// phone backed out and stops waiting. A failure here only costs the operator a
/// fall-through to their own timeout, so it must not mask the original abort.
async fn record_device_decline(state: &PairingHostDeps, rendezvous_id: &str) {
    if let Err(e) = state
        .device_pairing
        .set_device_decision(rendezvous_id, false)
        .await
    {
        tracing::debug!(error = %e, "device pair: failed to record device decline on slot");
    }
}

/// The pairing handshake's transport seam: send/recv one [`PairFrame`] with a
/// per-step receive timeout. Implemented in [`super::relay_pair`] for the
/// outbound relay leg the operator's `baybo device pair` hosts.
#[async_trait::async_trait]
pub(crate) trait PairTransport {
    async fn recv_frame(&mut self, timeout: Duration) -> Result<PairFrame, String>;
    async fn send_frame(&mut self, frame: &PairFrame) -> Result<(), String>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TestGateway, build_test_deps};
    use baybo_store::DeviceStatus;
    use device_proto::noise::StaticKeypair;
    use device_proto::psk_pair::PairingSecret;
    use tokio::task::JoinHandle;

    /// Per-step receive budget for the scripted app side.
    const STEP: Duration = Duration::from_secs(5);

    /// An in-memory [`PairTransport`] pair: each end ships encoded frames to the
    /// other, mirroring the relay leg's byte transport without a socket. The
    /// server end is handed to [`drive`]; the client end scripts the app side.
    struct ChanTransport {
        tx: tokio::sync::mpsc::Sender<Vec<u8>>,
        rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    }

    #[async_trait::async_trait]
    impl PairTransport for ChanTransport {
        async fn recv_frame(&mut self, timeout: Duration) -> Result<PairFrame, String> {
            match tokio::time::timeout(timeout, self.rx.recv()).await {
                Ok(Some(b)) => pairing::decode(&b).map_err(|e| format!("decode: {e}")),
                Ok(None) => Err("peer closed during pairing".into()),
                Err(_) => Err("timed out waiting for pairing frame".into()),
            }
        }

        async fn send_frame(&mut self, frame: &PairFrame) -> Result<(), String> {
            let bytes = pairing::encode(frame).map_err(|e| format!("encode: {e}"))?;
            self.tx
                .send(bytes)
                .await
                .map_err(|_| "peer closed during pairing".to_string())
        }
    }

    fn duplex() -> (ChanTransport, ChanTransport) {
        let (a_tx, b_rx) = tokio::sync::mpsc::channel(8);
        let (b_tx, a_rx) = tokio::sync::mpsc::channel(8);
        (
            ChanTransport { tx: a_tx, rx: a_rx },
            ChanTransport { tx: b_tx, rx: b_rx },
        )
    }

    /// Build a self-contained [`PairingHostDeps`] over the test gateway's
    /// in-memory stores, plus the device-pairing service (the same `Arc`, so a
    /// test scripts the operator side and the handshake observes it).
    fn deps_from(tg: &TestGateway) -> (PairingHostDeps, Arc<DevicePairingService>) {
        let device_pairing = Arc::new(DevicePairingService::new(tg.deps.stores.device.clone()));
        let deps = PairingHostDeps {
            device_pairing: Arc::clone(&device_pairing),
            secret_vault: tg.deps.secret_vault.clone(),
            relay_url: String::new(),
            push_url: "https://push.test".into(),
            remote_api_key: String::new(),
        };
        (deps, device_pairing)
    }

    /// Spawn [`drive`] over the server end, sending a best-effort `Reject` on abort
    /// exactly as the real relay leg does, so the client observes the same closing
    /// frame.
    fn spawn_drive(
        mut server: ChanTransport,
        deps: PairingHostDeps,
    ) -> JoinHandle<Result<(), String>> {
        tokio::spawn(async move {
            let r = drive(&mut server, &deps).await;
            if let Err(reason) = &r {
                let _ = server
                    .send_frame(&PairFrame::Reject {
                        reason: reason.clone(),
                    })
                    .await;
            }
            r
        })
    }

    /// The app-side XXpsk0 handshake: drive `Hello`/`HandshakeReply`/
    /// `HandshakeFinal` over `client`, returning the transport (in transport
    /// mode) so the test can exchange `DeviceConfirm` / `GatewayWelcome`.
    /// `relay_url` must match the gateway's (the prologue binds it).
    async fn app_handshake(
        client: &mut ChanTransport,
        rendezvous_id: &str,
        secret: &PairingSecret,
        relay_url: &str,
        device_id: &str,
    ) -> PskTransport {
        let app_static = StaticKeypair::generate().unwrap();
        let prologue = build_prologue(rendezvous_id, relay_url);
        let mut hs =
            PskHandshake::start_initiator(&app_static.secret(), secret, &prologue).unwrap();

        let msg1 = hs.write_handshake(&[]).unwrap();
        client
            .send_frame(&PairFrame::Hello {
                rendezvous_id: rendezvous_id.to_string(),
                msg: msg1,
            })
            .await
            .unwrap();

        let PairFrame::HandshakeReply { msg } = client.recv_frame(STEP).await.unwrap() else {
            panic!("expected HandshakeReply");
        };
        hs.read_handshake(&msg).unwrap();

        let hello = DeviceHello {
            device_id: device_id.to_string(),
        };
        let msg3 = hs
            .write_handshake(&pairing::encode(&hello).unwrap())
            .unwrap();
        client
            .send_frame(&PairFrame::HandshakeFinal { msg: msg3 })
            .await
            .unwrap();

        hs.into_transport().unwrap()
    }

    /// The full handshake end to end, both ends confirm, and an approved device
    /// row + stored push key result.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_pairing_handshake_lands_approved_device() {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        let (deps, device_pairing) = deps_from(&tg);
        let device_store = tg.deps.stores.device.clone();
        let vault = tg.deps.secret_vault.clone();

        let (rid, secret) = device_pairing.mint().await.unwrap();
        // Operator confirms in their live session, up front (no timing race).
        device_pairing
            .set_operator_decision(&rid, true)
            .await
            .unwrap();

        let (server, mut client) = duplex();
        let drive_task = spawn_drive(server, deps);

        // The app's Ed25519 identity; its public half is the device_id.
        let app_ed = delegation::generate_signing_key();
        let device_id = delegation::device_id_for(&app_ed.verifying_key());

        let mut app = app_handshake(&mut client, &rid, &secret, "", &device_id).await;
        let app_static = *app.remote_static(); // A's static, learned in-band
        let confirm_code = derive_confirm_code(app.handshake_hash()).unwrap();

        // The gateway publishes the confirm code onto the slot after it reads
        // HandshakeFinal — poll until it lands, then assert it matches what the
        // app derived from `h` (both ends derive the same channel binding).
        let mut published = None;
        for _ in 0..100 {
            if let Some(slot) = device_pairing.claim_slot(&rid).await.unwrap()
                && slot.confirm_code.is_some()
            {
                published = slot.confirm_code;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(published.as_deref(), Some(confirm_code.as_str()));

        // Phone accepts → sealed DeviceConfirm.
        let confirm = app
            .write(&pairing::encode(&DeviceConfirm { accepted: true }).unwrap())
            .unwrap();
        client
            .send_frame(&PairFrame::Sealed { msg: confirm })
            .await
            .unwrap();

        // Sealed GatewayWelcome.
        let PairFrame::Sealed { msg } = client.recv_frame(STEP).await.unwrap() else {
            panic!("expected sealed GatewayWelcome");
        };
        let welcome: GatewayWelcome = pairing::decode(&app.read(&msg).unwrap()).unwrap();
        assert!(!welcome.auth_token.is_empty());
        assert_eq!(welcome.rendezvous_id, rid);
        assert_eq!(welcome.gateway_push_pubkey.len(), 32);

        // 6th message: the device delegates the gateway push key (signed under the
        // device identity whose public half is the device_id).
        let gw_push = delegation::verifying_key_from_bytes(&welcome.gateway_push_pubkey).unwrap();
        let deleg = delegation::sign_delegation(&app_ed, &gw_push);
        let deleg_frame = app
            .write(
                &pairing::encode(&DeviceDelegation {
                    delegation: deleg.to_bytes().to_vec(),
                })
                .unwrap(),
            )
            .unwrap();
        client
            .send_frame(&PairFrame::Sealed { msg: deleg_frame })
            .await
            .unwrap();

        drive_task.await.unwrap().unwrap();

        // An approved device row landed pinning the app's in-band static + the
        // active token, and the per-device push key (HKDF of `h`) is stored.
        let row = device_store.get(&device_id).await.unwrap().unwrap();
        assert_eq!(row.status, DeviceStatus::Approved);
        assert_eq!(row.push_url, "https://push.test");
        assert_eq!(
            row.auth_token_sha256,
            baybo_store::device::hash_auth_token(&welcome.auth_token),
            "the row stores the digest of the bearer the device was handed"
        );
        assert_ne!(
            row.auth_token_sha256, welcome.auth_token,
            "the plaintext bearer must never be what is persisted"
        );
        // The gateway pinned A's *peer* static — i.e. the app's static key.
        assert_eq!(row.device_pubkey.len(), 32);
        assert_ne!(
            row.device_pubkey,
            app_static.to_vec(),
            "the row pins the app static, not the gateway's"
        );
        let push_key = derive_push_key(app.handshake_hash()).unwrap();
        let pk = vault
            .get_secret(&format!("device.{device_id}.push_key"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pk.as_bytes(), push_key.as_slice());

        // The verified push delegation was persisted so the dispatcher can sign.
        let stored_deleg = vault
            .get_secret(&format!("device.{device_id}.push_delegation"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored_deleg.as_bytes(), deleg.to_bytes().as_slice());

        // The durable row retains the public rendezvous id it paired under.
        assert_eq!(row.rendezvous_id.as_deref(), Some(rid.as_str()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pairing_with_unknown_rendezvous_is_rejected() {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        let (deps, _device_pairing) = deps_from(&tg);
        let (server, mut client) = duplex();
        let drive_task = spawn_drive(server, deps);

        // A never-minted rendezvous id → the handshake aborts before any crypto.
        let app_static = StaticKeypair::generate().unwrap();
        let prologue = build_prologue("ghost-rid", "");
        let mut hs = PskHandshake::start_initiator(
            &app_static.secret(),
            &PairingSecret::generate(),
            &prologue,
        )
        .unwrap();
        let msg1 = hs.write_handshake(&[]).unwrap();
        client
            .send_frame(&PairFrame::Hello {
                rendezvous_id: "ghost-rid".into(),
                msg: msg1,
            })
            .await
            .unwrap();
        match client.recv_frame(STEP).await {
            Ok(PairFrame::Reject { .. }) => {}
            other => panic!("expected Reject, got {other:?}"),
        }
        assert!(drive_task.await.unwrap().is_err());
    }

    /// A relay that knows the public rendezvous id but not the QR secret cannot
    /// complete the handshake — the PSK mismatch aborts it (MITM → DoS). The
    /// gateway can't even open the app's `msg1` (its empty payload is AEAD-sealed
    /// under a key derived from the PSK), so it rejects before `HandshakeReply`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wrong_secret_is_rejected() {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        let (deps, device_pairing) = deps_from(&tg);
        let (server, mut client) = duplex();
        let drive_task = spawn_drive(server, deps);

        let (rid, _real_secret) = device_pairing.mint().await.unwrap();
        // The attacker uses the right rendezvous id but a guessed secret.
        let app_static = StaticKeypair::generate().unwrap();
        let prologue = build_prologue(&rid, "");
        let mut hs = PskHandshake::start_initiator(
            &app_static.secret(),
            &PairingSecret::generate(),
            &prologue,
        )
        .unwrap();
        let msg1 = hs.write_handshake(&[]).unwrap();
        client
            .send_frame(&PairFrame::Hello {
                rendezvous_id: rid,
                msg: msg1,
            })
            .await
            .unwrap();
        match client.recv_frame(STEP).await {
            Ok(PairFrame::Reject { .. }) => {}
            other => panic!("expected Reject on a wrong PSK, got {other:?}"),
        }
        assert!(
            drive_task.await.unwrap().is_err(),
            "a wrong PSK must abort pairing"
        );
    }

    /// Drive the app through the handshake and stop just before the mutual-confirm
    /// step. Returns the test gateway, the client transport + its noise session,
    /// the rendezvous id, the pairing service, and the running `drive` task.
    async fn drive_to_confirm() -> (
        TestGateway,
        ChanTransport,
        PskTransport,
        String,
        Arc<DevicePairingService>,
        JoinHandle<Result<(), String>>,
    ) {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        let (deps, device_pairing) = deps_from(&tg);
        let (rid, secret) = device_pairing.mint().await.unwrap();

        let (server, mut client) = duplex();
        let drive_task = spawn_drive(server, deps);
        let app = app_handshake(&mut client, &rid, &secret, "", "dev-xyz").await;

        (tg, client, app, rid, device_pairing, drive_task)
    }

    /// Direction ②: the operator declines while the phone is still on the confirm
    /// screen. The handshake must notice the operator decline *concurrently* and
    /// reject the phone promptly, rather than blocking on a phone tap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn operator_decline_before_phone_confirm_rejects_promptly() {
        let (_tg, mut client, _app, rid, device_pairing, drive_task) = drive_to_confirm().await;
        device_pairing
            .set_operator_decision(&rid, false)
            .await
            .unwrap();
        match client.recv_frame(STEP).await {
            Ok(PairFrame::Reject { .. }) => {}
            other => panic!("expected prompt Reject, got {other:?}"),
        }
        assert!(drive_task.await.unwrap().is_err());
    }

    /// Direction ①: the phone user declines. The handshake aborts, rejects the
    /// phone, and records a device-side decline on the slot so the operator's
    /// polling `device pair` learns the device backed out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn phone_decline_records_device_decision_on_slot() {
        let (_tg, mut client, mut app, rid, device_pairing, drive_task) = drive_to_confirm().await;
        let confirm = app
            .write(&pairing::encode(&DeviceConfirm { accepted: false }).unwrap())
            .unwrap();
        client
            .send_frame(&PairFrame::Sealed { msg: confirm })
            .await
            .unwrap();
        match client.recv_frame(STEP).await {
            Ok(PairFrame::Reject { .. }) => {}
            other => panic!("expected Reject, got {other:?}"),
        }
        let slot = device_pairing.claim_slot(&rid).await.unwrap().unwrap();
        assert_eq!(slot.device_decision, Some(false));
        assert_eq!(slot.operator_decision, None);
        assert!(drive_task.await.unwrap().is_err());
    }
}
