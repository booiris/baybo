//! Token-free `/v1/device/pair` WebSocket: the SPAKE2 device-pairing handshake
//! with a Bluetooth-style mutual confirm.
//!
//! Pairing happens *before* any auth token exists, so this route carries no
//! channel-token middleware — it is gated by the SPAKE2 code itself (a balanced
//! PAKE allows one online guess per run) and then by a two-sided confirm: the
//! phone user and the operator each approve a confirmation code derived from K
//! before any token activates. The handshake (see
//! [`device_proto::pairing::PairFrame`]):
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

use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message as AxumWsMessage, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use device_proto::kdf::{derive_confirm_code, derive_pair_keys};
use device_proto::pairing::{self, DeviceConfirm, DeviceHello, GatewayWelcome, PairFrame};
use device_proto::pake::Pake;

use super::state::WsChannelState;
use crate::device::load_or_create_static_keypair;

/// Per-step receive timeout — a stalled peer must not pin a connection.
const PAIR_STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the confirm step waits on a human (the phone user's tap and the
/// operator's `y`) — longer than a protocol step since people are deciding.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(120);

/// Poll cadence for the operator's confirm decision on the shared slot.
const CONFIRM_POLL_INTERVAL: Duration = Duration::from_millis(400);

pub(crate) fn routes() -> Router<WsChannelState> {
    Router::new().route("/device/pair", get(handler))
}

async fn handler(State(state): State<WsChannelState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| run_pairing(socket, state))
}

async fn run_pairing(mut socket: WebSocket, state: WsChannelState) {
    if let Err(reason) = drive(&mut socket, &state).await {
        tracing::debug!(reason = %reason, "device pairing handshake aborted");
        // Best-effort reject so a well-behaved peer stops waiting; the socket
        // then closes on drop.
        let _ = send(&mut socket, &PairFrame::Reject { reason }).await;
    }
}

async fn drive(socket: &mut WebSocket, state: &WsChannelState) -> Result<(), String> {
    // 1. Hello — the pairing code + the app's SPAKE2 message.
    let PairFrame::Hello { code, pake } = recv(socket, PAIR_STEP_TIMEOUT).await? else {
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
    send(socket, &PairFrame::PakeReply { pake: gw_pake }).await?;

    // 4. Sealed DeviceHello — app static key + push registration.
    let PairFrame::Sealed { nonce, ciphertext } = recv(socket, PAIR_STEP_TIMEOUT).await? else {
        return Err("expected sealed DeviceHello".into());
    };
    let hello: DeviceHello = pairing::open_msg(&keys.channel_key, &nonce, &ciphertext)
        .map_err(|e| format!("open DeviceHello: {e}"))?;

    // 5. Mutual confirm. Publish the confirmation code (derived from K) onto the
    //    slot so the operator's live `device pair` shows it, then require BOTH
    //    the phone user and the operator to approve before any token activates.
    let confirm_code = derive_confirm_code(&k).map_err(|e| format!("confirm code: {e}"))?;
    state
        .device_pairing
        .publish_confirm(&slot.code, &confirm_code, &hello.device_id)
        .await
        .map_err(|e| format!("publish confirm: {e}"))?;

    // 5a. The phone user's decision, sealed under the channel key.
    let PairFrame::Sealed { nonce, ciphertext } = recv(socket, CONFIRM_TIMEOUT).await? else {
        return Err("expected sealed DeviceConfirm".into());
    };
    let confirm: DeviceConfirm = pairing::open_msg(&keys.channel_key, &nonce, &ciphertext)
        .map_err(|e| format!("open DeviceConfirm: {e}"))?;
    if !confirm.accepted {
        return Err("the device declined pairing".into());
    }

    // 5b. The operator's decision, polled from the shared slot (their live
    //     `device pair` writes it). Abandoned slots age out via the TTL.
    if !wait_operator_decision(state, &slot.code).await? {
        return Err("the operator declined pairing".into());
    }

    // 6. Finalize: write an approved device row + consume the slot.
    let row = state
        .device_pairing
        .complete(&slot, &hello.device_id, hello.static_pubkey.to_vec())
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
    // Gateway-mediated APNs registration: relay the device's APNs token to the
    // remote host (C), authenticated by A's instance key. Best-effort — pairing
    // already succeeded; a failed registration is retried by the dispatcher
    // from the persisted material, and is skipped entirely when push isn't
    // configured.
    if let Some(registrar) = &state.apns_registrar
        && let Err(e) = registrar
            .register_device(&hello.device_id, &hello.apns_token, hello.apns_env)
            .await
    {
        tracing::warn!(error = %e, "device APNs registration with remote host failed");
    }

    // 7. A's static Noise identity (lazily loaded/created from the vault).
    let static_key = load_or_create_static_keypair(&state.secret_vault)
        .await
        .map_err(|e| format!("static key: {e}"))?;

    // 8. Sealed GatewayWelcome — A's static key, routing (empty until the
    //    relay/direct config lands), and the issued (inert) auth_token.
    let welcome = GatewayWelcome {
        static_pubkey: static_key.public(),
        // relay_node_id is C-assigned (phase 2, when A holds its control
        // connection to the remote host); direct candidates come from config.
        relay_node_id: String::new(),
        direct_candidates: state.device_direct_candidates.clone(),
        user_id: slot.user_id.clone(),
        pairing_code: slot.code.clone(),
        auth_token: row.auth_token.clone(),
    };
    let (nonce, ciphertext) =
        pairing::seal_msg(&keys.channel_key, &welcome).map_err(|e| format!("seal welcome: {e}"))?;
    send(socket, &PairFrame::Sealed { nonce, ciphertext }).await?;

    tracing::info!(
        device = %super::short_hash(&hello.device_id),
        user = %super::short_hash(&slot.user_id),
        "device paired",
    );
    Ok(())
}

/// Poll the shared slot for the operator's confirm decision (their live
/// `device pair` writes it) until it is set or the confirm window elapses.
async fn wait_operator_decision(state: &WsChannelState, code: &str) -> Result<bool, String> {
    let deadline = tokio::time::Instant::now() + CONFIRM_TIMEOUT;
    loop {
        let slot = state
            .device_pairing
            .claim_slot(code)
            .await
            .map_err(|e| format!("poll operator decision: {e}"))?
            .ok_or_else(|| "pairing slot expired before the operator decided".to_string())?;
        if let Some(decision) = slot.operator_decision {
            return Ok(decision);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("timed out waiting for the operator to confirm".into());
        }
        tokio::time::sleep(CONFIRM_POLL_INTERVAL).await;
    }
}

async fn recv(socket: &mut WebSocket, timeout: Duration) -> Result<PairFrame, String> {
    let next = tokio::time::timeout(timeout, socket.recv())
        .await
        .map_err(|_| "timed out waiting for pairing frame".to_string())?;
    let msg = next
        .ok_or_else(|| "peer closed during pairing".to_string())?
        .map_err(|e| format!("ws error: {e}"))?;
    match msg {
        AxumWsMessage::Binary(bytes) => {
            pairing::decode(&bytes).map_err(|e| format!("decode pairing frame: {e}"))
        }
        AxumWsMessage::Close(_) => Err("peer closed during pairing".into()),
        other => Err(format!("expected binary pairing frame, got {other:?}")),
    }
}

async fn send(socket: &mut WebSocket, frame: &PairFrame) -> Result<(), String> {
    let bytes = pairing::encode(frame).map_err(|e| format!("encode pairing frame: {e}"))?;
    socket
        .send(AxumWsMessage::Binary(bytes.into()))
        .await
        .map_err(|e| format!("send pairing frame: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::build_test_deps;
    use baybo_store::DeviceStatus;
    use device_proto::kdf::derive_pair_keys;
    use device_proto::noise::StaticKeypair;
    use device_proto::pairing::ApnsEnv;
    use futures::{SinkExt, StreamExt};
    use tokio::net::TcpStream;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::client_async;
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    type Client = WebSocketStream<TcpStream>;

    async fn connect(port: u16) -> Client {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let req = format!("ws://127.0.0.1:{port}/v1/device/pair")
            .into_client_request()
            .unwrap();
        client_async(req, stream).await.unwrap().0
    }

    async fn send_pf(ws: &mut Client, frame: &PairFrame) {
        ws.send(WsMessage::Binary(pairing::encode(frame).unwrap()))
            .await
            .unwrap();
    }

    async fn recv_pf(ws: &mut Client) -> PairFrame {
        loop {
            match ws.next().await.unwrap().unwrap() {
                WsMessage::Binary(b) => return pairing::decode(&b).unwrap(),
                WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
                other => panic!("unexpected ws frame: {other:?}"),
            }
        }
    }

    /// The full SPAKE2 handshake over a real WebSocket: an app-side client
    /// drives it end to end, both ends confirm, and an approved device row +
    /// stored push key result.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_pairing_handshake_lands_approved_device() {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        let state = WsChannelState::from_deps(&tg.deps);
        let device_store = tg.deps.stores.device.clone();
        let vault = tg.deps.secret_vault.clone();
        let device_pairing = state.device_pairing.clone();

        // Operator mints a slot (the `baybo device pair` step).
        let code = device_pairing.mint("user-1", "Test iPhone").await.unwrap();
        // Operator confirms in their live session (their CLI writes this). Set
        // up front so the gateway's poll finds it without a timing race.
        device_pairing.set_operator_decision(&code, true).await.unwrap();

        // Serve just the token-free pairing route on an ephemeral port.
        let app = Router::new().nest("/v1", routes().with_state(state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });

        let mut ws = connect(port).await;

        // 1–2: SPAKE2 (app side).
        let (pake, app_msg) = Pake::start_app(&code);
        send_pf(
            &mut ws,
            &PairFrame::Hello {
                code: code.clone(),
                pake: app_msg,
            },
        )
        .await;
        let PairFrame::PakeReply { pake: gw_pake } = recv_pf(&mut ws).await else {
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
        send_pf(&mut ws, &PairFrame::Sealed { nonce, ciphertext }).await;

        // 3b: sealed DeviceConfirm — the phone user accepts the shown code.
        let confirm = DeviceConfirm { accepted: true };
        let (nonce, ciphertext) = pairing::seal_msg(&keys.channel_key, &confirm).unwrap();
        send_pf(&mut ws, &PairFrame::Sealed { nonce, ciphertext }).await;

        // 4: sealed GatewayWelcome.
        let PairFrame::Sealed { nonce, ciphertext } = recv_pf(&mut ws).await else {
            panic!("expected sealed GatewayWelcome");
        };
        let welcome: GatewayWelcome =
            pairing::open_msg(&keys.channel_key, &nonce, &ciphertext).unwrap();
        assert_eq!(welcome.user_id, "user-1");
        assert!(!welcome.auth_token.is_empty());
        assert_eq!(welcome.static_pubkey.len(), 32);

        // An approved device row landed with the app's static key + the active
        // token, and the per-device push key (HKDF of K) is stored.
        let row = device_store
            .get("user-1", "dev-xyz")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, DeviceStatus::Approved);
        assert_eq!(row.auth_token, welcome.auth_token);
        assert_eq!(row.device_pubkey, device_static.public().to_vec());
        let pk = vault
            .get_secret("device.dev-xyz.push_key")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pk.as_bytes(), keys.push_key.as_slice());

        // The durable device row retains the code it paired under (slot deletion
        // is covered by the service tests).
        assert_eq!(row.pairing_code.as_deref(), Some(code.as_str()));
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pairing_with_bad_code_is_rejected() {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        let state = WsChannelState::from_deps(&tg.deps);
        let app = Router::new().nest("/v1", routes().with_state(state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });

        let mut ws = connect(port).await;
        let (_pake, app_msg) = Pake::start_app("NEVER1");
        send_pf(
            &mut ws,
            &PairFrame::Hello {
                code: "NEVER1".into(),
                pake: app_msg,
            },
        )
        .await;
        // Never-minted code → Reject.
        match recv_pf(&mut ws).await {
            PairFrame::Reject { .. } => {}
            other => panic!("expected Reject, got {other:?}"),
        }
        server.abort();
    }
}
