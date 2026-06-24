//! Token-authenticated `/v1/device/content` WebSocket: the gateway side of a
//! paired device's **content session**.
//!
//! Pairing (`device_pair`) leaves the device holding A's static Noise key and an
//! `auth_token`; A holds the device's static key in the device row. This route
//! is where the device cashes that in to actually chat:
//!
//! 1. The channel-auth middleware resolves the device's `auth_token` to
//!    [`AuthedClient::Device`] (an *approved* device row) before the upgrade.
//! 2. A runs a **Noise IK responder** handshake (its static key from
//!    [`load_or_create_static_keypair`]) and verifies the initiator's static
//!    equals the device row's `device_pubkey` — so a leaked token alone can't
//!    open the session, and C (a relay) can't MITM it.
//! 3. After transport mode, every WS binary frame is Noise-decrypted into a
//!    [`wire::Frame`], fed into the *same* channel frame loop the TUI / web chat
//!    use ([`super::route::run_inbound_loop`]), and every reply is
//!    Noise-encrypted on the way out.
//!
//! So this module is just a Noise-wrapping transport ([`NoiseFrameSink`] /
//! [`NoiseFrameSource`]) around the existing `Subscribed`-channel machinery: the
//! device registers as [`ChannelType::ios`], `Subscribe`s a session, and
//! self-pulls / sends like any other subscribed connection.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message as AxumWsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, State};
use axum::response::IntoResponse;
use axum::routing::get;
use baybo_channels::wire::{self, Frame};
use baybo_model::ChannelType;
use device_proto::aead::KEY_LEN;
use futures::stream::SplitStream;
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use snow::TransportState;

use super::adapter::{FrameSink, FrameSource, Sidecar, WsSink};
use super::state::WsChannelState;
use crate::auth::{AuthedClient, constant_time_eq};
use crate::device::load_or_create_static_keypair;

/// snow's per-message ceiling; a Noise transport message (handshake or
/// post-handshake frame) cannot exceed this. Matches `baybo-mobile-core`.
const MAX_NOISE_MSG: usize = 65535;
/// ChaCha20-Poly1305 auth tag length added to every sealed transport message.
const NOISE_TAG_LEN: usize = 16;
/// How long the responder waits for the initiator's first handshake message
/// after the WS upgrade — a stalled peer must not pin a connection.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn routes() -> Router<WsChannelState> {
    Router::new().route("/device/content", get(handler))
}

async fn handler(
    State(state): State<WsChannelState>,
    Extension(authed): Extension<AuthedClient>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| run_content(socket, state, authed))
}

async fn run_content(socket: WebSocket, state: WsChannelState, authed: AuthedClient) {
    let AuthedClient::Device { user_id, device_id } = authed else {
        // Channel auth resolves any valid channel token here, but only a paired
        // device may open a content session — everyone else is rejected.
        tracing::warn!("non-device client on /v1/device/content; closing");
        return;
    };
    if let Err(reason) = drive_content(socket, &state, &user_id, &device_id).await {
        tracing::debug!(
            device = %super::short_hash(&device_id),
            reason = %reason,
            "device content session aborted",
        );
    }
}

async fn drive_content(
    socket: WebSocket,
    state: &WsChannelState,
    user_id: &str,
    device_id: &str,
) -> Result<(), String> {
    // The device row carries the static key the device proved it owned at
    // pairing; the IK handshake below must match it. (The auth middleware
    // already confirmed the row is *approved* by resolving the token.)
    let row = state
        .device_store
        .get(user_id, device_id)
        .await
        .map_err(|e| format!("device lookup: {e}"))?
        .ok_or_else(|| "device row vanished after auth".to_string())?;

    let gateway_static = load_or_create_static_keypair(&state.secret_vault)
        .await
        .map_err(|e| format!("gateway static key: {e}"))?;

    let (mut sink, mut source) = socket.split();

    // Noise IK responder handshake (2 messages): read the initiator's `msg1`
    // (which carries its static key), verify it against the device row, then
    // send `msg2` and enter transport mode.
    let mut handshake = gateway_static
        .ik_responder()
        .map_err(|e| format!("build ik responder: {e}"))?;
    let msg1 = recv_binary(&mut source, HANDSHAKE_TIMEOUT).await?;
    let mut buf = vec![0u8; MAX_NOISE_MSG];
    handshake
        .read_message(&msg1, &mut buf)
        .map_err(|e| format!("read handshake msg1: {e}"))?;

    let remote_static = handshake
        .get_remote_static()
        .ok_or_else(|| "initiator sent no static key".to_string())?;
    if remote_static.len() != KEY_LEN || !constant_time_eq(remote_static, &row.device_pubkey) {
        return Err("initiator static key does not match the paired device".into());
    }

    let n = handshake
        .write_message(&[], &mut buf)
        .map_err(|e| format!("write handshake msg2: {e}"))?;
    sink.send(AxumWsMessage::Binary(buf[..n].to_vec().into()))
        .await
        .map_err(|e| format!("send handshake msg2: {e}"))?;
    let transport = handshake
        .into_transport_mode()
        .map_err(|e| format!("enter transport mode: {e}"))?;
    let transport = Arc::new(Mutex::new(transport));

    tracing::info!(
        device = %super::short_hash(device_id),
        user = %super::short_hash(user_id),
        "device content session established",
    );

    // Best-effort liveness bump for the operator's device list.
    let now = chrono::Utc::now().timestamp();
    if let Err(e) = state
        .device_store
        .touch_last_seen(user_id, device_id, now)
        .await
    {
        tracing::debug!(error = %e, "touch device last_seen failed");
    }

    // From here on the device is an ordinary `Subscribed`-channel connection,
    // just with a Noise-wrapped transport: reuse the channel registry + the
    // shared inbound loop so `Subscribe` catch-up, live fan-out, and inbound
    // `Message` routing all work exactly as they do for the TUI / web chat.
    let channel_type = ChannelType::ios();
    let channel = super::adapter::resolve_or_install_channel(&state.registry, &channel_type)
        .map_err(|e| format!("resolve ios channel: {e}"))?;

    let sidecar = Sidecar::build(
        channel_type.clone(),
        channel,
        NoiseFrameSink {
            inner: sink,
            transport: Arc::clone(&transport),
        },
        Arc::clone(&state.blob_store),
    );

    super::route::run_inbound_loop(
        NoiseFrameSource {
            source,
            transport: Arc::clone(&transport),
        },
        state,
        &channel_type,
        &sidecar,
    )
    .await;

    let _ = sidecar.into_pump().await;
    tracing::info!(
        device = %super::short_hash(device_id),
        "device content session closed",
    );
    Ok(())
}

/// Read the next binary WS message (skipping any ping/pong), with a timeout.
/// Used for the two Noise handshake messages before the frame loop takes over.
async fn recv_binary(
    source: &mut SplitStream<WebSocket>,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    loop {
        let next = tokio::time::timeout(timeout, source.next())
            .await
            .map_err(|_| "timed out waiting for handshake message".to_string())?;
        match next {
            Some(Ok(AxumWsMessage::Binary(bytes))) => return Ok(bytes.into()),
            Some(Ok(AxumWsMessage::Ping(_) | AxumWsMessage::Pong(_))) => continue,
            Some(Ok(AxumWsMessage::Close(_))) | None => {
                return Err("peer closed during handshake".into());
            }
            Some(Ok(other)) => {
                return Err(format!("expected binary handshake frame, got {other:?}"));
            }
            Some(Err(e)) => return Err(format!("ws error during handshake: {e}")),
        }
    }
}

/// Outbound: Noise-encrypt each encoded [`Frame`] before it rides the WS.
pub(crate) struct NoiseFrameSink {
    inner: WsSink,
    transport: Arc<Mutex<TransportState>>,
}

#[async_trait::async_trait]
impl FrameSink for NoiseFrameSink {
    async fn send_frame(&mut self, frame: &Frame) -> Result<(), ()> {
        let plaintext = match wire::encode(frame) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "encode outbound content frame");
                return Ok(());
            }
        };
        // A single frame larger than the Noise per-message ceiling can't be
        // sealed; skip it rather than tear down an otherwise-healthy session.
        if plaintext.len() > MAX_NOISE_MSG - NOISE_TAG_LEN {
            tracing::warn!(
                len = plaintext.len(),
                "content frame exceeds the Noise message ceiling; dropping",
            );
            return Ok(());
        }
        let ciphertext = {
            let mut buf = vec![0u8; plaintext.len() + NOISE_TAG_LEN];
            let mut transport = self.transport.lock();
            match transport.write_message(&plaintext, &mut buf) {
                Ok(n) => {
                    buf.truncate(n);
                    buf
                }
                Err(e) => {
                    tracing::warn!(error = %e, "noise encrypt failed; pump exiting");
                    return Err(());
                }
            }
        };
        self.inner
            .send(AxumWsMessage::Binary(ciphertext.into()))
            .await
            .map_err(|e| tracing::debug!(error = %e, "content ws sink error; pump exiting"))
    }

    async fn close(&mut self) {
        let _ = self.inner.close().await;
    }
}

/// Inbound: Noise-decrypt each WS binary message into an encoded [`Frame`].
pub(crate) struct NoiseFrameSource {
    source: SplitStream<WebSocket>,
    transport: Arc<Mutex<TransportState>>,
}

#[async_trait::async_trait]
impl FrameSource for NoiseFrameSource {
    async fn next_frame(&mut self) -> Option<Frame> {
        loop {
            let msg = match self.source.next().await {
                Some(Ok(m)) => m,
                Some(Err(e)) => {
                    tracing::debug!(error = %e, "content ws read error; tearing down");
                    return None;
                }
                None => return None,
            };
            match msg {
                AxumWsMessage::Binary(bytes) => {
                    // A Noise transport message can't exceed snow's ceiling, and
                    // `read_message` would reject an oversized one anyway — bail
                    // before allocating a buffer sized to an attacker-chosen frame.
                    if bytes.len() > MAX_NOISE_MSG {
                        tracing::warn!(
                            len = bytes.len(),
                            "content frame exceeds the Noise ceiling; closing"
                        );
                        return None;
                    }
                    // Plaintext is never longer than the ciphertext.
                    let mut buf = vec![0u8; bytes.len()];
                    let plaintext_len = {
                        let mut transport = self.transport.lock();
                        match transport.read_message(&bytes, &mut buf) {
                            Ok(n) => n,
                            Err(e) => {
                                // A decrypt failure means the stream desynced —
                                // unrecoverable for Noise, so tear the session down.
                                tracing::warn!(error = %e, "noise decrypt failed; tearing down");
                                return None;
                            }
                        }
                    };
                    match wire::decode(&buf[..plaintext_len]) {
                        Ok(frame) => return Some(frame),
                        Err(e) => {
                            tracing::warn!(error = %e, "decode content frame failed");
                            continue;
                        }
                    }
                }
                AxumWsMessage::Close(_) => return None,
                AxumWsMessage::Ping(_) | AxumWsMessage::Pong(_) => continue,
                AxumWsMessage::Text(_) => {
                    tracing::warn!("unexpected text frame on content ws; closing");
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{ChannelAuthState, attach_channel_auth};
    use crate::test_support::build_test_deps;
    use baybo_channels::RouterInbound;
    use baybo_channels::wire::{Message as WireMessage, MessageRole};
    use baybo_store::{DeviceRow, DeviceStatus};
    use device_proto::noise::StaticKeypair;
    use futures::{SinkExt, StreamExt};
    use snow::HandshakeState;
    use tokio::net::TcpStream;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::client_async;
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    type Client = WebSocketStream<TcpStream>;

    const TEST_TOKEN: &str = "device-auth-token-fixed-0123456789abcdef";

    fn device_row(user_id: &str, device_id: &str, pubkey: Vec<u8>) -> DeviceRow {
        DeviceRow {
            user_id: user_id.into(),
            device_id: device_id.into(),
            label: "Test iPhone".into(),
            device_pubkey: pubkey,
            auth_token: TEST_TOKEN.into(),
            status: DeviceStatus::Approved,
            pairing_code: Some("WORMHOLE-1".into()),
            created_at: 0,
            approved_at: Some(0),
            last_seen_at: None,
        }
    }

    /// Serve the content route (behind channel auth with device-store lookup) on
    /// an ephemeral port, returning the port + the gateway's static pubkey.
    async fn serve(tg: &crate::test_support::TestGateway) -> (u16, [u8; KEY_LEN]) {
        crate::channel::boot::install_channel(&tg.deps.channel_registry, ChannelType::ios())
            .expect("install ios channel");
        // Persist (and read) A's static key so the client can target it.
        let gw_static = load_or_create_static_keypair(&tg.deps.secret_vault)
            .await
            .expect("gateway static key");
        let state = WsChannelState::from_deps(&tg.deps);
        let auth_state = ChannelAuthState::new(tg.deps.channel_tokens.clone())
            .with_device_store(tg.deps.stores.device.clone());
        let app = Router::new().nest(
            "/v1",
            attach_channel_auth(routes().with_state(state), auth_state),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });
        (port, gw_static.public())
    }

    async fn connect(port: u16, token: &str) -> Client {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let req = format!("ws://127.0.0.1:{port}/v1/device/content?token={token}")
            .into_client_request()
            .unwrap();
        client_async(req, stream).await.unwrap().0
    }

    async fn recv_bin(ws: &mut Client) -> Option<Vec<u8>> {
        loop {
            match ws.next().await {
                Some(Ok(WsMessage::Binary(b))) => return Some(b.to_vec()),
                Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_))) => continue,
                Some(Ok(WsMessage::Close(_))) | None => return None,
                Some(Ok(_)) => continue,
                Some(Err(_)) => return None,
            }
        }
    }

    /// Drive the IK initiator side (mirror of `baybo-mobile-core`'s
    /// `ContentHandshake`) over a live socket; returns the device transport.
    async fn client_handshake(
        ws: &mut Client,
        device: &StaticKeypair,
        gateway_pubkey: &[u8; KEY_LEN],
    ) -> Option<TransportState> {
        let mut hs: HandshakeState = device.ik_initiator(gateway_pubkey).unwrap();
        let mut buf = vec![0u8; MAX_NOISE_MSG];
        let n = hs.write_message(&[], &mut buf).unwrap();
        ws.send(WsMessage::Binary(buf[..n].to_vec())).await.unwrap();
        let msg2 = recv_bin(ws).await?;
        hs.read_message(&msg2, &mut buf).ok()?;
        hs.into_transport_mode().ok()
    }

    fn seal(transport: &mut TransportState, frame: &Frame) -> Vec<u8> {
        let plaintext = wire::encode(frame).unwrap();
        let mut buf = vec![0u8; plaintext.len() + NOISE_TAG_LEN];
        let n = transport.write_message(&plaintext, &mut buf).unwrap();
        buf.truncate(n);
        buf
    }

    fn open(transport: &mut TransportState, ciphertext: &[u8]) -> Frame {
        let mut buf = vec![0u8; ciphertext.len()];
        let n = transport.read_message(ciphertext, &mut buf).unwrap();
        wire::decode(&buf[..n]).unwrap()
    }

    /// Full content session: an approved device authenticates, completes the
    /// Noise IK handshake, subscribes, and sends a message — which reaches the
    /// router intake *and* is echoed back to the device over Noise.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn device_subscribes_sends_and_self_pulls_over_noise() {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        let device = StaticKeypair::generate().unwrap();
        tg.deps
            .stores
            .device
            .create(&device_row("user-1", "ios-dev", device.public().to_vec()))
            .await
            .expect("seed approved device row");

        let (port, gw_pubkey) = serve(&tg).await;
        let mut incoming_rx = tg.incoming_rx;

        let mut ws = connect(port, TEST_TOKEN).await;
        let mut transport = client_handshake(&mut ws, &device, &gw_pubkey)
            .await
            .expect("noise handshake completes");

        // Subscribe, then send a user message on that session.
        let sealed = seal(
            &mut transport,
            &Frame::Subscribe {
                session_id: "sess-1".into(),
                since_ordinal: None,
            },
        );
        ws.send(WsMessage::Binary(sealed)).await.unwrap();

        let msg = Frame::Message(WireMessage {
            content: "hi from the phone".into(),
            session_id: "sess-1".into(),
            user_id: "user-1".into(),
            channel_type: ChannelType::ios(),
            bot_id: String::new(),
            attachments: Vec::new(),
            platform_msg_id: "m1".into(),
            role: MessageRole::User,
            ordinal: None,
        });
        let sealed = seal(&mut transport, &msg);
        ws.send(WsMessage::Binary(sealed)).await.unwrap();

        // The gateway routed the decrypted inbound message to the agent intake.
        let inbound = tokio::time::timeout(Duration::from_secs(2), incoming_rx.recv())
            .await
            .expect("router intake within timeout")
            .expect("router intake item");
        match inbound {
            RouterInbound::One(incoming) => {
                assert_eq!(incoming.message.content.len(), 1);
                assert_eq!(incoming.message.session_id.as_str(), "sess-1");
            }
            other => panic!("expected RouterInbound::One, got {other:?}"),
        }

        // …and the user-echo of that message comes back to the device, proving
        // the outbound Noise path through the real Sidecar pump. Skip the
        // post-Subscribe snapshot frames until the echoed Message lands.
        let mut saw_echo = false;
        for _ in 0..8 {
            let Some(bytes) = recv_bin(&mut ws).await else {
                break;
            };
            if let Frame::Message(m) = open(&mut transport, &bytes) {
                assert_eq!(m.content, "hi from the phone");
                assert!(matches!(m.role, MessageRole::User));
                saw_echo = true;
                break;
            }
        }
        assert!(saw_echo, "device never received the Noise-wrapped echo");
    }

    /// An initiator whose static key isn't the paired device's is rejected at
    /// the handshake — the gateway closes before transport mode, so the client
    /// never gets a usable session. Guards the IK static-key binding.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wrong_device_static_key_is_rejected() {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        let paired = StaticKeypair::generate().unwrap();
        let imposter = StaticKeypair::generate().unwrap();
        // The row binds the *paired* device's pubkey; the client below presents
        // the imposter key under the same (valid) auth token.
        tg.deps
            .stores
            .device
            .create(&device_row("user-1", "ios-dev", paired.public().to_vec()))
            .await
            .expect("seed approved device row");

        let (port, gw_pubkey) = serve(&tg).await;
        let mut ws = connect(port, TEST_TOKEN).await;
        // The imposter completes its half of the IK math locally, but the
        // gateway aborts after msg1 (static mismatch), so no msg2 arrives.
        assert!(
            client_handshake(&mut ws, &imposter, &gw_pubkey)
                .await
                .is_none(),
            "handshake must not complete for a non-paired static key",
        );
    }
}
