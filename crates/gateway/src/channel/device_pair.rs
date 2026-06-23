//! Token-free `/v1/device/pair` WebSocket: the SPAKE2 device-pairing handshake.
//!
//! Pairing happens *before* any auth token exists, so this route carries no
//! channel-token middleware — it is gated by the SPAKE2 code itself (a balanced
//! PAKE allows one online guess per run; the operator's `aura device approve`
//! is the second gate). The 4-message handshake (see
//! [`device_proto::pairing::PairFrame`]):
//!
//! 1. P → A `Hello { code, pake }`   — A claims the slot and runs SPAKE2.
//! 2. A → P `PakeReply { pake }`     — both ends derive the master secret K.
//! 3. P → A `Sealed(DeviceHello)`    — app's static key + push registration.
//! 4. A → P `Sealed(GatewayWelcome)` — A's static key, routing, `auth_token`.
//!
//! On success a **pending** device row exists (its `auth_token` inert until
//! the operator approves) and the per-device push key (HKDF of K) is stored in
//! A's `SecretVault`. C never sees any of this — it only relays opaque blobs.

use std::time::Duration;

use device_proto::kdf::derive_pair_keys;
use device_proto::pairing::{self, DeviceHello, GatewayWelcome, PairFrame};
use device_proto::pake::Pake;
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message as AxumWsMessage, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;

use super::state::WsChannelState;
use crate::device::load_or_create_static_keypair;

/// Per-step receive timeout — a stalled peer must not pin a connection.
const PAIR_STEP_TIMEOUT: Duration = Duration::from_secs(30);

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
    let PairFrame::Hello { code, pake } = recv(socket).await? else {
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
    let PairFrame::Sealed { nonce, ciphertext } = recv(socket).await? else {
        return Err("expected sealed DeviceHello".into());
    };
    let hello: DeviceHello = pairing::open_msg(&keys.channel_key, &nonce, &ciphertext)
        .map_err(|e| format!("open DeviceHello: {e}"))?;

    // 5. Finalize: write a pending device row + consume the slot.
    let row = state
        .device_pairing
        .complete(&slot, &hello.device_id, hello.static_pubkey.to_vec())
        .await
        .map_err(|e| format!("complete pairing: {e}"))?;

    // 6. Persist the per-device push key (HKDF of K) in A's vault, keyed by
    //    device_id (the NSE selects it by `bid` at push time).
    let push_key_name = format!("device.{}.push_key", hello.device_id);
    state
        .secret_vault
        .store_secret(&push_key_name, &keys.push_key)
        .await
        .map_err(|e| format!("store push key: {e}"))?;
    // Gateway-mediated APNs registration: relay the device's APNs token to the
    // remote host (C), authenticated by A's instance key. Best-effort — pairing
    // already succeeded; a failed registration only delays pushes until the
    // next attempt, and is skipped entirely when push isn't configured.
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
        "device paired (pending operator approval)",
    );
    Ok(())
}

async fn recv(socket: &mut WebSocket) -> Result<PairFrame, String> {
    let next = tokio::time::timeout(PAIR_STEP_TIMEOUT, socket.recv())
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
    use device_proto::kdf::derive_pair_keys;
    use device_proto::noise::StaticKeypair;
    use device_proto::pairing::ApnsEnv;
    use aura_store::DeviceStatus;
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
    /// drives it end to end and a pending device row + stored push key result.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_pairing_handshake_lands_pending_device() {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        let state = WsChannelState::from_deps(&tg.deps);
        let device_store = tg.deps.stores.device.clone();
        let vault = tg.deps.secret_vault.clone();

        // Operator mints a slot (the `aura device pair` step).
        let code = state.device_pairing.mint("user-1", "Test iPhone").await.unwrap();

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

        // 4: sealed GatewayWelcome.
        let PairFrame::Sealed { nonce, ciphertext } = recv_pf(&mut ws).await else {
            panic!("expected sealed GatewayWelcome");
        };
        let welcome: GatewayWelcome =
            pairing::open_msg(&keys.channel_key, &nonce, &ciphertext).unwrap();
        assert_eq!(welcome.user_id, "user-1");
        assert!(!welcome.auth_token.is_empty());
        assert_eq!(welcome.static_pubkey.len(), 32);

        // A pending device row landed with the app's static key + the issued
        // (inert) token, and the per-device push key (HKDF of K) is stored.
        let row = device_store.get("user-1", "dev-xyz").await.unwrap().unwrap();
        assert_eq!(row.status, DeviceStatus::Pending);
        assert_eq!(row.auth_token, welcome.auth_token);
        assert_eq!(row.device_pubkey, device_static.public().to_vec());
        let pk = vault
            .get_secret("device.dev-xyz.push_key")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pk.as_bytes(), keys.push_key.as_slice());

        // Slot consumed: the durable device row carries the code as the
        // approval handle now (slot deletion is covered by the service tests).
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
