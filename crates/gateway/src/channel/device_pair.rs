//! The SPAKE2 device-pairing handshake ([`drive`]) with a Bluetooth-style mutual
//! confirm, plus the in-process [`PairingHostDeps`] it runs against.
//!
//! Pairing is driven entirely by one interactive `baybo device pair`: it hosts an
//! outbound relay leg (see [`super::relay_pair::host_pairing_leg`]) and runs this
//! handshake over it. There is no daemon-served pairing route — the gateway only
//! serves *content* for already-paired devices.
//!
//! Pairing happens *before* any auth token exists, so it carries no channel-token
//! middleware — it is gated by the SPAKE2 code itself (a balanced PAKE allows one
//! online guess per run) and then by a two-sided confirm: the phone user and the
//! operator each approve a confirmation code derived from K before any token
//! activates. The handshake (see [`device_proto::pairing::PairFrame`]):
//!
//! 1. P → A `Hello { code, pake }` — A claims the slot and runs SPAKE2.
//! 2. A → P `PakeReply { pake }` — both ends derive the master secret K + the confirmation code (HKDF of K).
//! 3. P → A `Sealed(DeviceHello)` — app's static key + push registration.
//! 4. P → A `Sealed(DeviceConfirm)` — the phone user's decision; A also waits for the operator's `y` on the live `device pair`.
//! 5. A → P `Sealed(GatewayWelcome)` — A's static key, routing, active `auth_token`.
//!
//! On mutual confirm an **approved** device row exists (its `auth_token` active)
//! and the per-device push key (HKDF of K) is stored in A's `SecretVault`. C
//! never sees any of this — it only relays opaque blobs.

use std::sync::Arc;
use std::time::Duration;

use device_proto::kdf::{derive_confirm_code, derive_pair_keys};
use device_proto::pairing::{self, DeviceConfirm, DeviceHello, GatewayWelcome, PairFrame};
use device_proto::pake::Pake;

use baybo_pairing::DevicePairingService;
use baybo_security::SecretVault;

use crate::device::load_or_create_static_keypair;

/// The slice of gateway state the pairing handshake ([`drive`]) needs. Carried
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
    pub relay_url: String,
}

/// Per-step receive timeout — a stalled peer must not pin a connection.
const PAIR_STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the confirm step waits on a human (the phone user's tap and the
/// operator's `y`) — longer than a protocol step since people are deciding.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(120);

/// Poll cadence for the operator's confirm decision on the shared slot.
const CONFIRM_POLL_INTERVAL: Duration = Duration::from_millis(400);

pub(crate) async fn drive<T: PairTransport + ?Sized>(
    transport: &mut T,
    state: &PairingHostDeps,
) -> Result<(), String> {
    // 1. Hello — the pairing code + the app's SPAKE2 message.
    let PairFrame::Hello { code, pake } = transport.recv_frame(PAIR_STEP_TIMEOUT).await? else {
        return Err("expected Hello frame".into());
    };

    // 2. Authorize against the live slot, then run SPAKE2 (gateway side).
    let slot = state
        .device_pairing
        .claim_slot(&code)
        .await
        .map_err(|e| format!("claim slot: {e}"))?
        .ok_or_else(|| "unknown or expired pairing code".to_string())?;
    let (pake_state, gw_pake) = Pake::start_gateway(&code);
    let k = pake_state.finish(&pake).map_err(|e| format!("pake: {e}"))?;
    let keys = derive_pair_keys(&k).map_err(|e| format!("kdf: {e}"))?;

    // 3. Hand the app our SPAKE2 message; it derives the same K.
    transport
        .send_frame(&PairFrame::PakeReply { pake: gw_pake })
        .await?;

    // 4. Sealed DeviceHello — app static key + push registration.
    let PairFrame::Sealed { nonce, ciphertext } = transport.recv_frame(PAIR_STEP_TIMEOUT).await?
    else {
        return Err("expected sealed DeviceHello".into());
    };
    let hello: DeviceHello = pairing::open_msg(&keys.channel_key, &nonce, &ciphertext)
        .map_err(|e| format!("open DeviceHello: {e}"))?;

    // 5. Mutual confirm. Publish the confirmation code (derived from K) onto the
    //    slot so the operator's live `device pair` shows it, then require BOTH
    //    the phone user and the operator to approve before any token activates.
    // Resolve the device label: the operator's `device pair <label>` override if
    // they passed one, else the name the device reports in `DeviceHello`.
    let device_label = if slot.label.is_empty() {
        hello.label.as_str()
    } else {
        slot.label.as_str()
    };
    let confirm_code = derive_confirm_code(&k).map_err(|e| format!("confirm code: {e}"))?;
    state
        .device_pairing
        .publish_confirm(&slot.code, &confirm_code, &hello.device_id, device_label)
        .await
        .map_err(|e| format!("publish confirm: {e}"))?;

    // 5a/5b. Mutual confirm, awaited *concurrently*: the phone user's decision
    //   arrives as a sealed DeviceConfirm over the transport, while the operator's
    //   decision is polled from the shared slot (their live `device pair` writes
    //   it). Both must be "accept" — but the moment either side declines (or the
    //   phone drops) we abort so the *other* side isn't left hanging: the phone
    //   learns via the Reject `run_pairing`/`host_leg_once` sends on our `Err`, and
    //   the operator learns via the `device_decision` we record on the slot. (The
    //   old serial order — phone first, then operator — meant an operator decline
    //   went unseen until the phone tapped, and a phone decline never reached the
    //   operator at all.)
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
                frame = &mut phone_confirm, if !phone_ok => match phone_decision(frame, &keys.channel_key) {
                    PhoneDecision::Accepted => phone_ok = true,
                    PhoneDecision::Declined => {
                        record_device_decline(state, &slot.code).await;
                        return Err("the device declined pairing".into());
                    }
                    PhoneDecision::Aborted(reason) => {
                        record_device_decline(state, &slot.code).await;
                        return Err(reason);
                    }
                },
                // The operator's decision on the shared slot.
                _ = tokio::time::sleep(CONFIRM_POLL_INTERVAL), if !operator_ok => {
                    let decision = state
                        .device_pairing
                        .claim_slot(&slot.code)
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

    // 6. Finalize: write an approved device row + consume the slot.
    let row = state
        .device_pairing
        .complete(
            &slot,
            &hello.device_id,
            hello.static_pubkey.to_vec(),
            device_label,
        )
        .await
        .map_err(|e| format!("complete pairing: {e}"))?;

    // 6. Persist the per-device push key (HKDF of K) in A's vault, keyed by
    //    device_id (the NSE selects it by `bid` at push time).
    let push_key_name = crate::push::device_push_key_secret_name(&hello.device_id);
    state
        .secret_vault
        .store_secret(&push_key_name, &keys.push_key)
        .await
        .map_err(|e| format!("store push key: {e}"))?;
    // 6b. Persist the APNs registration material (token + env) in the vault so a
    //     transient `/register` failure below is retriable — the push dispatcher
    //     re-registers an approved device from this before its first push.
    let apns_reg = crate::push::DeviceApnsRegistration {
        apns_token: hello.apns_token.clone(),
        apns_env: hello.apns_env,
    };
    let apns_bytes =
        serde_json::to_vec(&apns_reg).map_err(|e| format!("encode apns registration: {e}"))?;
    state
        .secret_vault
        .store_secret(
            &crate::push::device_apns_secret_name(&hello.device_id),
            &apns_bytes,
        )
        .await
        .map_err(|e| format!("store apns registration: {e}"))?;
    // The token persisted above is enough: the daemon's push dispatcher registers
    // the device with the remote host (C) from that material before its first
    // push. (Pairing runs in `baybo device pair`, which has no dispatcher.)

    // 7. A's static Noise identity (lazily loaded/created from the vault).
    let static_key = load_or_create_static_keypair(&state.secret_vault)
        .await
        .map_err(|e| format!("static key: {e}"))?;

    // 8. Sealed GatewayWelcome — A's static key + the routing the app reaches it
    //    by: direct candidates from config, and (when a relay is configured) the
    //    gateway's stable relay_node_id + the relay's base URL so the app can
    //    fall back to the blind relay when every direct candidate fails.
    let (relay_node_id, relay_url) = if state.relay_url.is_empty() {
        (String::new(), String::new())
    } else {
        let node_id = crate::relay::load_or_create_relay_node_id(&state.secret_vault)
            .await
            .map_err(|e| format!("relay node id: {e}"))?;
        (node_id, state.relay_url.clone())
    };
    let welcome = GatewayWelcome {
        static_pubkey: static_key.public(),
        relay_node_id,
        relay_url,
        user_id: slot.user_id.clone(),
        pairing_code: slot.code.clone(),
        auth_token: row.auth_token.clone(),
    };
    let (nonce, ciphertext) =
        pairing::seal_msg(&keys.channel_key, &welcome).map_err(|e| format!("seal welcome: {e}"))?;
    transport
        .send_frame(&PairFrame::Sealed { nonce, ciphertext })
        .await?;

    tracing::info!(
        device = %super::short_hash(&hello.device_id),
        user = %super::short_hash(&slot.user_id),
        "device paired",
    );
    Ok(())
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

/// Open the phone's confirm frame and classify the decision. `frame` is the raw
/// receive result, so a transport error (timeout / closed socket) maps to
/// [`PhoneDecision::Aborted`] rather than being lost.
fn phone_decision(
    frame: Result<PairFrame, String>,
    channel_key: &[u8; device_proto::aead::KEY_LEN],
) -> PhoneDecision {
    let frame = match frame {
        Ok(frame) => frame,
        Err(reason) => return PhoneDecision::Aborted(reason),
    };
    let PairFrame::Sealed { nonce, ciphertext } = frame else {
        return PhoneDecision::Aborted("expected sealed DeviceConfirm".into());
    };
    match pairing::open_msg::<DeviceConfirm>(channel_key, &nonce, &ciphertext) {
        Ok(confirm) if confirm.accepted => PhoneDecision::Accepted,
        Ok(_) => PhoneDecision::Declined,
        Err(e) => PhoneDecision::Aborted(format!("open DeviceConfirm: {e}")),
    }
}

/// Best-effort: mark the slot so the operator's live `device pair` learns the
/// phone backed out and stops waiting. A failure here only costs the operator a
/// fall-through to their own timeout, so it must not mask the original abort.
async fn record_device_decline(state: &PairingHostDeps, code: &str) {
    if let Err(e) = state.device_pairing.set_device_decision(code, false).await {
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
    use device_proto::kdf::PairKeys;
    use device_proto::noise::StaticKeypair;
    use device_proto::pairing::ApnsEnv;
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

    /// The full SPAKE2 handshake over the in-memory transport: an app-side client
    /// drives it end to end, both ends confirm, and an approved device row +
    /// stored push key result.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_pairing_handshake_lands_approved_device() {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        let (deps, device_pairing) = deps_from(&tg);
        let device_store = tg.deps.stores.device.clone();
        let vault = tg.deps.secret_vault.clone();

        // Operator mints a slot with no label (the bare `baybo device pair`): the
        // device's reported name should flow through instead.
        let code = device_pairing.mint("user-1", "").await.unwrap();
        // Operator confirms in their live session. Set up front so the poll finds
        // it without a timing race.
        device_pairing
            .set_operator_decision(&code, true)
            .await
            .unwrap();

        let (server, mut client) = duplex();
        let drive_task = spawn_drive(server, deps);

        // 1–2: SPAKE2 (app side).
        let (pake, app_msg) = Pake::start_app(&code);
        client
            .send_frame(&PairFrame::Hello {
                code: code.clone(),
                pake: app_msg,
            })
            .await
            .unwrap();
        let PairFrame::PakeReply { pake: gw_pake } = client.recv_frame(STEP).await.unwrap() else {
            panic!("expected PakeReply");
        };
        let k = pake.finish(&gw_pake).unwrap();
        let keys = derive_pair_keys(&k).unwrap();

        // 3: sealed DeviceHello.
        let device_static = StaticKeypair::generate().unwrap();
        let hello = DeviceHello {
            device_id: "dev-xyz".into(),
            label: "Test iPhone".into(),
            static_pubkey: device_static.public(),
            apns_token: "apns-tok".into(),
            apns_env: ApnsEnv::Sandbox,
        };
        let (nonce, ciphertext) = pairing::seal_msg(&keys.channel_key, &hello).unwrap();
        client
            .send_frame(&PairFrame::Sealed { nonce, ciphertext })
            .await
            .unwrap();

        // 3b: sealed DeviceConfirm — the phone user accepts the shown code.
        let confirm = DeviceConfirm { accepted: true };
        let (nonce, ciphertext) = pairing::seal_msg(&keys.channel_key, &confirm).unwrap();
        client
            .send_frame(&PairFrame::Sealed { nonce, ciphertext })
            .await
            .unwrap();

        // 4: sealed GatewayWelcome.
        let PairFrame::Sealed { nonce, ciphertext } = client.recv_frame(STEP).await.unwrap() else {
            panic!("expected sealed GatewayWelcome");
        };
        let welcome: GatewayWelcome =
            pairing::open_msg(&keys.channel_key, &nonce, &ciphertext).unwrap();
        assert_eq!(welcome.user_id, "user-1");
        assert!(!welcome.auth_token.is_empty());
        assert_eq!(welcome.static_pubkey.len(), 32);
        drive_task.await.unwrap().unwrap();

        // An approved device row landed with the app's static key + the active
        // token, and the per-device push key (HKDF of K) is stored.
        let row = device_store
            .get("user-1", "dev-xyz")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, DeviceStatus::Approved);
        assert_eq!(
            row.label, "Test iPhone",
            "no operator label → the device's reported name is used"
        );
        assert_eq!(row.auth_token, welcome.auth_token);
        assert_eq!(row.device_pubkey, device_static.public().to_vec());
        let pk = vault
            .get_secret("device.dev-xyz.push_key")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pk.as_bytes(), keys.push_key.as_slice());

        // The durable device row retains the code it paired under.
        assert_eq!(row.pairing_code.as_deref(), Some(code.as_str()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pairing_with_bad_code_is_rejected() {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        let (deps, _device_pairing) = deps_from(&tg);
        let (server, mut client) = duplex();
        let drive_task = spawn_drive(server, deps);

        let (_pake, app_msg) = Pake::start_app("NEVER1");
        client
            .send_frame(&PairFrame::Hello {
                code: "NEVER1".into(),
                pake: app_msg,
            })
            .await
            .unwrap();
        // Never-minted code → the handshake aborts and the leg rejects.
        match client.recv_frame(STEP).await {
            Ok(PairFrame::Reject { .. }) => {}
            other => panic!("expected Reject, got {other:?}"),
        }
        assert!(drive_task.await.unwrap().is_err());
    }

    /// Drive a client through SPAKE2 + the sealed `DeviceHello` and stop just
    /// before the mutual-confirm step. Returns the test gateway (kept alive for
    /// its libsql tempdir), the client transport, the derived keys, the slot code,
    /// the pairing service (to script the operator side / inspect the slot), and
    /// the running `drive` task.
    async fn drive_to_confirm() -> (
        TestGateway,
        ChanTransport,
        PairKeys,
        String,
        Arc<DevicePairingService>,
        JoinHandle<Result<(), String>>,
    ) {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        let (deps, device_pairing) = deps_from(&tg);
        let code = device_pairing.mint("user-1", "").await.unwrap();

        let (server, mut client) = duplex();
        let drive_task = spawn_drive(server, deps);

        let (pake, app_msg) = Pake::start_app(&code);
        client
            .send_frame(&PairFrame::Hello {
                code: code.clone(),
                pake: app_msg,
            })
            .await
            .unwrap();
        let PairFrame::PakeReply { pake: gw_pake } = client.recv_frame(STEP).await.unwrap() else {
            panic!("expected PakeReply");
        };
        let k = pake.finish(&gw_pake).unwrap();
        let keys = derive_pair_keys(&k).unwrap();

        let device_static = StaticKeypair::generate().unwrap();
        let hello = DeviceHello {
            device_id: "dev-xyz".into(),
            label: "Test iPhone".into(),
            static_pubkey: device_static.public(),
            apns_token: "apns-tok".into(),
            apns_env: ApnsEnv::Sandbox,
        };
        let (nonce, ciphertext) = pairing::seal_msg(&keys.channel_key, &hello).unwrap();
        client
            .send_frame(&PairFrame::Sealed { nonce, ciphertext })
            .await
            .unwrap();

        (tg, client, keys, code, device_pairing, drive_task)
    }

    /// Direction ②: the operator declines while the phone is still on the confirm
    /// screen (no `DeviceConfirm` sent yet). The handshake must notice the operator
    /// decline *concurrently* and reject the phone promptly, rather than blocking
    /// on a phone tap that may never come (the old serial order did).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn operator_decline_before_phone_confirm_rejects_promptly() {
        let (_tg, mut client, _keys, code, device_pairing, drive_task) = drive_to_confirm().await;
        // Operator declines on their terminal; the phone has not tapped.
        device_pairing
            .set_operator_decision(&code, false)
            .await
            .unwrap();
        // The handshake aborts and rejects the phone without ever seeing a confirm
        // (the `STEP` recv budget also asserts it doesn't block on the phone tap).
        match client.recv_frame(STEP).await {
            Ok(PairFrame::Reject { .. }) => {}
            other => panic!("expected prompt Reject, got {other:?}"),
        }
        assert!(drive_task.await.unwrap().is_err());
    }

    /// Direction ①: the phone user declines. The handshake aborts, rejects the
    /// phone, and records a device-side decline on the slot so the operator's
    /// polling `device pair` learns the device backed out instead of waiting out
    /// its own timeout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn phone_decline_records_device_decision_on_slot() {
        let (_tg, mut client, keys, code, device_pairing, drive_task) = drive_to_confirm().await;
        // The phone user taps Cancel.
        let (nonce, ciphertext) =
            pairing::seal_msg(&keys.channel_key, &DeviceConfirm { accepted: false }).unwrap();
        client
            .send_frame(&PairFrame::Sealed { nonce, ciphertext })
            .await
            .unwrap();
        // The handshake rejects the phone …
        match client.recv_frame(STEP).await {
            Ok(PairFrame::Reject { .. }) => {}
            other => panic!("expected Reject, got {other:?}"),
        }
        // … and the device-side decline is now visible to the operator's poll (the
        // slot survives — only a successful pair consumes it).
        let slot = device_pairing.claim_slot(&code).await.unwrap().unwrap();
        assert_eq!(slot.device_decision, Some(false));
        assert_eq!(slot.operator_decision, None);
        assert!(drive_task.await.unwrap().is_err());
    }
}
