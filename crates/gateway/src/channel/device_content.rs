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
use device_proto::noise::{FrameReassembler, NOISE_MAX_MESSAGE, write_chunked};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use snow::TransportState;
use tokio_tungstenite::tungstenite::Message as TungMessage;

/// An outbound relay data leg (the gateway dialed C's `/content/host/{key}`).
type RelayWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

use super::adapter::{FrameSink, FrameSource, Sidecar, WsSink};
use super::state::WsChannelState;
use crate::auth::{AuthedClient, constant_time_eq};
use crate::device::load_or_create_static_keypair;

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
    // The auth middleware already confirmed the row is *approved* by resolving
    // the token; pin the IK initiator's static to *this* device's pubkey so a
    // valid token can't be paired with a different device's key.
    let row = match state.device_store.get(&user_id, &device_id).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            tracing::warn!("device row vanished after auth; closing");
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "device lookup failed; closing");
            return;
        }
    };
    let (sink, source) = socket.split();
    let auth = DeviceAuth::Pinned {
        device_pubkey: row.device_pubkey,
        user_id,
        device_id,
    };
    if let Err(reason) =
        run_content_session(AxumBinSink(sink), AxumBinSource(source), &state, auth).await
    {
        tracing::debug!(reason = %reason, "device content session aborted");
    }
}

/// Run the content responder over an outbound **relay** data leg (the gateway
/// dialed C's `/content/host/{relay_key}` after a control-plane signal). No
/// prior channel-auth ran — the gateway dialed blind — so the device is
/// authenticated purely by matching the Noise IK initiator's static to an
/// approved device row.
pub(crate) async fn run_content_over_relay(ws: RelayWs, state: &WsChannelState) {
    let (sink, source) = ws.split();
    if let Err(reason) = run_content_session(
        TungBinSink(sink),
        TungBinSource(source),
        state,
        DeviceAuth::AnyApproved,
    )
    .await
    {
        tracing::debug!(reason = %reason, "relay content session aborted");
    }
}

/// How the responder authenticates the Noise IK initiator's static key.
enum DeviceAuth {
    /// Direct path: the WS was already device-token-authed; the initiator's
    /// static must equal exactly this device's pubkey.
    Pinned {
        device_pubkey: Vec<u8>,
        user_id: String,
        device_id: String,
    },
    /// Relay path: no prior auth; accept any *approved* device whose pubkey
    /// matches the initiator's static (resolved by key).
    AnyApproved,
}

/// The transport-agnostic content responder: run the Noise IK responder
/// handshake (authenticating the device per `auth`), then Noise-wrap the shared
/// channel frame loop over `sink`/`source`. Used by both the direct axum WS and
/// the outbound relay data leg.
async fn run_content_session<Si: BinarySink, So: BinarySource>(
    mut sink: Si,
    mut source: So,
    state: &WsChannelState,
    auth: DeviceAuth,
) -> Result<(), String> {
    let gateway_static = load_or_create_static_keypair(&state.secret_vault)
        .await
        .map_err(|e| format!("gateway static key: {e}"))?;

    // Noise IK responder handshake (2 messages): read the initiator's `msg1`
    // (which carries its static key), authenticate it, then send `msg2` and
    // enter transport mode.
    let mut handshake = gateway_static
        .ik_responder()
        .map_err(|e| format!("build ik responder: {e}"))?;
    let msg1 = recv_handshake(&mut source, HANDSHAKE_TIMEOUT).await?;
    let mut buf = vec![0u8; NOISE_MAX_MESSAGE];
    handshake
        .read_message(&msg1, &mut buf)
        .map_err(|e| format!("read handshake msg1: {e}"))?;
    let remote_static = handshake
        .get_remote_static()
        .ok_or_else(|| "initiator sent no static key".to_string())?
        .to_vec();

    let (user_id, device_id) = match auth {
        DeviceAuth::Pinned {
            device_pubkey,
            user_id,
            device_id,
        } => {
            if remote_static.len() != KEY_LEN || !constant_time_eq(&remote_static, &device_pubkey) {
                return Err("initiator static key does not match the paired device".into());
            }
            (user_id, device_id)
        }
        DeviceAuth::AnyApproved => {
            let row = state
                .device_store
                .lookup_approved_by_pubkey(&remote_static)
                .await
                .map_err(|e| format!("device lookup by pubkey: {e}"))?
                .ok_or_else(|| "no approved device for this static key".to_string())?;
            (row.user_id, row.device_id)
        }
    };

    let n = handshake
        .write_message(&[], &mut buf)
        .map_err(|e| format!("write handshake msg2: {e}"))?;
    sink.send_bytes(buf[..n].to_vec())
        .await
        .map_err(|()| "send handshake msg2".to_string())?;
    let transport = handshake
        .into_transport_mode()
        .map_err(|e| format!("enter transport mode: {e}"))?;
    let transport = Arc::new(Mutex::new(transport));

    tracing::info!(
        device = %super::short_hash(&device_id),
        user = %super::short_hash(&user_id),
        "device content session established",
    );

    // Best-effort liveness bump for the operator's device list.
    let now = chrono::Utc::now().timestamp();
    if let Err(e) = state
        .device_store
        .touch_last_seen(&user_id, &device_id, now)
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
            reassembler: FrameReassembler::new(),
            pending: std::collections::VecDeque::new(),
        },
        state,
        &channel_type,
        &sidecar,
    )
    .await;

    let _ = sidecar.into_pump().await;
    tracing::info!(device = %super::short_hash(&device_id), "device content session closed");
    Ok(())
}

/// Read the next binary message (skipping ping/pong) with a timeout — for the
/// two Noise handshake messages before the frame loop takes over.
async fn recv_handshake<So: BinarySource>(
    source: &mut So,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    tokio::time::timeout(timeout, source.next_bytes())
        .await
        .map_err(|_| "timed out waiting for handshake message".to_string())?
        .ok_or_else(|| "peer closed during handshake".to_string())
}

/// A binary-message duplex the content responder runs over: the direct axum WS
/// ([`AxumBinSink`]/[`AxumBinSource`]) or an outbound relay data leg
/// ([`TungBinSink`]/[`TungBinSource`]). Only opaque binary frames cross it (Noise
/// ciphertext); ping/pong are skipped, anything else ends the stream.
#[async_trait::async_trait]
pub(crate) trait BinarySink: Send + 'static {
    /// Send one binary message. `Err(())` means the wire is gone.
    async fn send_bytes(&mut self, bytes: Vec<u8>) -> Result<(), ()>;
    async fn close(&mut self);
}

#[async_trait::async_trait]
pub(crate) trait BinarySource: Send {
    /// Next binary message, or `None` on close / read error.
    async fn next_bytes(&mut self) -> Option<Vec<u8>>;
}

/// Direct path: the axum WebSocket's split halves.
struct AxumBinSink(WsSink);
struct AxumBinSource(SplitStream<WebSocket>);

#[async_trait::async_trait]
impl BinarySink for AxumBinSink {
    async fn send_bytes(&mut self, bytes: Vec<u8>) -> Result<(), ()> {
        self.0
            .send(AxumWsMessage::Binary(bytes.into()))
            .await
            .map_err(|e| tracing::debug!(error = %e, "content ws sink error"))
    }
    async fn close(&mut self) {
        let _ = self.0.close().await;
    }
}

#[async_trait::async_trait]
impl BinarySource for AxumBinSource {
    async fn next_bytes(&mut self) -> Option<Vec<u8>> {
        loop {
            match self.0.next().await {
                Some(Ok(AxumWsMessage::Binary(b))) => return Some(b.into()),
                Some(Ok(AxumWsMessage::Ping(_) | AxumWsMessage::Pong(_))) => continue,
                Some(Ok(_)) | Some(Err(_)) | None => return None,
            }
        }
    }
}

/// Relay path: the outbound `tokio-tungstenite` data leg's split halves.
struct TungBinSink(SplitSink<RelayWs, TungMessage>);
struct TungBinSource(SplitStream<RelayWs>);

#[async_trait::async_trait]
impl BinarySink for TungBinSink {
    async fn send_bytes(&mut self, bytes: Vec<u8>) -> Result<(), ()> {
        self.0
            .send(TungMessage::Binary(bytes))
            .await
            .map_err(|e| tracing::debug!(error = %e, "relay leg sink error"))
    }
    async fn close(&mut self) {
        let _ = self.0.close().await;
    }
}

#[async_trait::async_trait]
impl BinarySource for TungBinSource {
    async fn next_bytes(&mut self) -> Option<Vec<u8>> {
        loop {
            match self.0.next().await {
                Some(Ok(TungMessage::Binary(b))) => return Some(b),
                Some(Ok(TungMessage::Ping(_) | TungMessage::Pong(_))) => continue,
                Some(Ok(_)) | Some(Err(_)) | None => return None,
            }
        }
    }
}

/// Outbound: Noise-encrypt each encoded [`Frame`] before it rides the transport.
pub(crate) struct NoiseFrameSink<S: BinarySink> {
    inner: S,
    transport: Arc<Mutex<TransportState>>,
}

#[async_trait::async_trait]
impl<S: BinarySink> FrameSink for NoiseFrameSink<S> {
    async fn send_frame(&mut self, frame: &Frame) -> Result<(), ()> {
        let plaintext = match wire::encode(frame) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "encode outbound content frame");
                return Ok(());
            }
        };
        // Chunk past the Noise per-message ceiling: a large frame seals into
        // several transport messages the peer reassembles in order.
        let messages = {
            let mut transport = self.transport.lock();
            match write_chunked(&mut transport, &plaintext) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(error = %e, "noise encrypt failed; pump exiting");
                    return Err(());
                }
            }
        };
        for message in messages {
            self.inner.send_bytes(message).await?;
        }
        Ok(())
    }

    async fn close(&mut self) {
        self.inner.close().await;
    }
}

/// Inbound: Noise-decrypt each binary message into an encoded [`Frame`].
pub(crate) struct NoiseFrameSource<R: BinarySource> {
    source: R,
    transport: Arc<Mutex<TransportState>>,
    reassembler: FrameReassembler,
    /// Frames reassembled from inbound messages but not yet handed out (the
    /// `FrameSource` returns them one at a time).
    pending: std::collections::VecDeque<Frame>,
}

#[async_trait::async_trait]
impl<R: BinarySource> FrameSource for NoiseFrameSource<R> {
    async fn next_frame(&mut self) -> Option<Frame> {
        loop {
            if let Some(frame) = self.pending.pop_front() {
                return Some(frame);
            }
            let bytes = self.source.next_bytes().await?;
            let reassembled = {
                let mut transport = self.transport.lock();
                match self.reassembler.read(&mut transport, &bytes) {
                    Ok(frames) => frames,
                    Err(e) => {
                        // A decrypt failure / desync is unrecoverable for Noise,
                        // so tear the session down.
                        tracing::warn!(error = %e, "noise decrypt failed; tearing down");
                        return None;
                    }
                }
            };
            for frame_bytes in reassembled {
                match wire::decode(&frame_bytes) {
                    Ok(frame) => self.pending.push_back(frame),
                    Err(e) => tracing::warn!(error = %e, "decode content frame failed"),
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
        let mut buf = vec![0u8; NOISE_MAX_MESSAGE];
        let n = hs.write_message(&[], &mut buf).unwrap();
        ws.send(WsMessage::Binary(buf[..n].to_vec())).await.unwrap();
        let msg2 = recv_bin(ws).await?;
        hs.read_message(&msg2, &mut buf).ok()?;
        hs.into_transport_mode().ok()
    }

    fn seal(transport: &mut TransportState, frame: &Frame) -> Vec<u8> {
        let plaintext = wire::encode(frame).unwrap();
        let mut messages = write_chunked(transport, &plaintext).unwrap();
        assert_eq!(messages.len(), 1, "test frames fit one chunk");
        messages.remove(0)
    }

    fn open(transport: &mut TransportState, ciphertext: &[u8]) -> Frame {
        // The test frames are single-chunk, so a fresh reassembler per message
        // yields exactly one frame (the Noise nonce lives in `transport`).
        let mut reassembler = FrameReassembler::new();
        let mut frames = reassembler.read(transport, ciphertext).unwrap();
        assert_eq!(frames.len(), 1, "test frames fit one chunk");
        wire::decode(&frames.remove(0)).unwrap()
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

    // In-memory binary legs standing in for a spliced relay data leg, so the
    // relay content path is testable without a live relay.
    struct ChanSink(tokio::sync::mpsc::Sender<Vec<u8>>);
    struct ChanSource(tokio::sync::mpsc::Receiver<Vec<u8>>);

    #[async_trait::async_trait]
    impl BinarySink for ChanSink {
        async fn send_bytes(&mut self, bytes: Vec<u8>) -> Result<(), ()> {
            self.0.send(bytes).await.map_err(|_| ())
        }
        async fn close(&mut self) {}
    }

    #[async_trait::async_trait]
    impl BinarySource for ChanSource {
        async fn next_bytes(&mut self) -> Option<Vec<u8>> {
            self.0.recv().await
        }
    }

    /// The relay content path: no channel-auth ran, so the responder resolves
    /// the device by *looking up* the IK initiator's static key (the
    /// `DeviceAuth::AnyApproved` branch), then routes + echoes a message over
    /// the generic binary transport. Mirrors the direct test with the auth path
    /// swapped and in-memory legs instead of a WS.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn relay_path_resolves_device_by_pubkey_and_round_trips() {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        let device = StaticKeypair::generate().unwrap();
        tg.deps
            .stores
            .device
            .create(&device_row("user-1", "ios-dev", device.public().to_vec()))
            .await
            .expect("seed approved device row");
        crate::channel::boot::install_channel(&tg.deps.channel_registry, ChannelType::ios())
            .expect("install ios channel");
        let gw_static = load_or_create_static_keypair(&tg.deps.secret_vault)
            .await
            .expect("gateway static key");
        let gw_pub = gw_static.public();
        let state = WsChannelState::from_deps(&tg.deps);
        let mut incoming_rx = tg.incoming_rx;

        // In-memory duplex: gateway content session <-> phone initiator.
        let (gw_tx, mut phone_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16); // gateway -> phone
        let (phone_tx, gw_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16); // phone -> gateway
        tokio::spawn(async move {
            let _ = run_content_session(
                ChanSink(gw_tx),
                ChanSource(gw_rx),
                &state,
                DeviceAuth::AnyApproved,
            )
            .await;
        });

        // The phone: IK initiator handshake, then Subscribe + a message.
        let mut hs: HandshakeState = device.ik_initiator(&gw_pub).unwrap();
        let mut buf = vec![0u8; NOISE_MAX_MESSAGE];
        let n = hs.write_message(&[], &mut buf).unwrap();
        phone_tx.send(buf[..n].to_vec()).await.unwrap();
        let msg2 = phone_rx.recv().await.expect("gateway msg2");
        hs.read_message(&msg2, &mut buf).unwrap();
        let mut transport = hs.into_transport_mode().unwrap();

        phone_tx
            .send(seal(
                &mut transport,
                &Frame::Subscribe {
                    session_id: "sess-r".into(),
                    since_ordinal: None,
                },
            ))
            .await
            .unwrap();
        let msg = Frame::Message(WireMessage {
            content: "via relay".into(),
            session_id: "sess-r".into(),
            user_id: "user-1".into(),
            channel_type: ChannelType::ios(),
            bot_id: String::new(),
            attachments: Vec::new(),
            platform_msg_id: "m1".into(),
            role: MessageRole::User,
            ordinal: None,
        });
        phone_tx.send(seal(&mut transport, &msg)).await.unwrap();

        // The relay-resolved device's message reaches the router intake.
        let inbound = tokio::time::timeout(Duration::from_secs(2), incoming_rx.recv())
            .await
            .expect("router intake within timeout")
            .expect("router intake item");
        match inbound {
            RouterInbound::One(incoming) => {
                assert_eq!(incoming.message.session_id.as_str(), "sess-r");
            }
            other => panic!("expected RouterInbound::One, got {other:?}"),
        }

        // …and the echo comes back over the in-memory relay legs.
        let mut saw_echo = false;
        for _ in 0..8 {
            let Some(bytes) = phone_rx.recv().await else {
                break;
            };
            if let Frame::Message(m) = open(&mut transport, &bytes) {
                assert_eq!(m.content, "via relay");
                saw_echo = true;
                break;
            }
        }
        assert!(
            saw_echo,
            "phone never received the echo over the relay legs"
        );
    }
}
