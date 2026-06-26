//! The gateway side of a paired device's **content session** (chat), run over the
//! relay's content data leg.
//!
//! Pairing (`device_pair`) leaves the device holding A's static Noise key; A holds
//! the device's static key in the device row. A NAT'd gateway can't be dialed, so
//! the device reaches it through C's blind relay: C splices the phone's
//! `/content/join/{node_id}` leg to the gateway's outbound `/content/host/{key}`
//! data leg (see [`super::relay_content`]), and the gateway runs a **Noise IK
//! responder** over it:
//!
//! 1. A reads the initiator's first handshake message (its static key) and
//!    authenticates the device by matching that static to an *approved* device
//!    row — no token rides the relay leg, and C (a relay) can't MITM it.
//! 2. After transport mode, every WS binary frame is Noise-decrypted into a
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

use baybo_channels::wire::{self, Frame};
use baybo_model::ChannelType;
use device_proto::noise::{FrameReassembler, NOISE_MAX_MESSAGE, write_chunked};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use snow::TransportState;
use tokio_tungstenite::tungstenite::Message as TungMessage;

/// An outbound relay data leg (the gateway dialed C's `/content/host/{key}`).
type RelayWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

use super::adapter::{FrameSink, FrameSource, Sidecar};
use super::state::WsChannelState;
use crate::device::load_or_create_static_keypair;

/// How long the responder waits for the initiator's first handshake message
/// after the WS upgrade — a stalled peer must not pin a connection.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Run the content responder over an outbound **relay** data leg (the gateway
/// dialed C's `/content/host/{relay_key}` after a control-plane signal). No
/// prior channel-auth ran — the gateway dialed blind — so the device is
/// authenticated purely by matching the Noise IK initiator's static to an
/// approved device row.
pub(crate) async fn run_content_over_relay(ws: RelayWs, state: &WsChannelState) {
    let (sink, source) = ws.split();
    if let Err(reason) = run_content_session(TungBinSink(sink), TungBinSource(source), state).await
    {
        tracing::debug!(reason = %reason, "relay content session aborted");
    }
}

/// The content responder: run the Noise IK responder handshake (authenticating
/// the device by matching the initiator's static key to an approved device row),
/// then Noise-wrap the shared channel frame loop over `sink`/`source`. Runs over
/// the outbound relay data leg.
async fn run_content_session<Si: BinarySink, So: BinarySource>(
    mut sink: Si,
    mut source: So,
    state: &WsChannelState,
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

    // No prior channel-auth ran (the gateway dialed the relay data leg blind), so
    // authenticate the device purely by matching the Noise IK initiator's static
    // key to an approved device row.
    let row = state
        .device_store
        .lookup_approved_by_pubkey(&remote_static)
        .await
        .map_err(|e| format!("device lookup by pubkey: {e}"))?
        .ok_or_else(|| "no approved device for this static key".to_string())?;
    let (user_id, device_id) = (row.user_id, row.device_id);

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

/// A binary-message duplex the content responder runs over: the outbound relay
/// data leg ([`TungBinSink`]/[`TungBinSource`]). Only opaque binary frames cross
/// it (Noise ciphertext); ping/pong are skipped, anything else ends the stream.
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

/// The outbound `tokio-tungstenite` relay data leg's split halves.
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
    use crate::test_support::build_test_deps;
    use baybo_channels::RouterInbound;
    use baybo_channels::wire::{Message as WireMessage, MessageRole};
    use baybo_store::{DeviceRow, DeviceStatus};
    use device_proto::noise::StaticKeypair;
    use snow::HandshakeState;

    const TEST_TOKEN: &str = "device-auth-token-fixed-0123456789abcdef";

    fn device_row(user_id: &str, device_id: &str, pubkey: Vec<u8>) -> DeviceRow {
        DeviceRow {
            user_id: user_id.into(),
            device_id: device_id.into(),
            device_pubkey: pubkey,
            auth_token: TEST_TOKEN.into(),
            status: DeviceStatus::Approved,
            rendezvous_id: Some("11111111-2222-4333-8444-555555555555".into()),
            created_at: 0,
            approved_at: Some(0),
            last_seen_at: None,
        }
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

    /// The content path: no channel-auth ran (the gateway dialed the relay data
    /// leg blind), so the responder resolves the device by *looking up* the IK
    /// initiator's static key, then routes + echoes a message over the generic
    /// binary transport (in-memory legs stand in for a spliced relay data leg).
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
            let _ = run_content_session(ChanSink(gw_tx), ChanSource(gw_rx), &state).await;
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

    /// A static key with no approved device row is rejected at the handshake: the
    /// responder errors before transport mode, so the initiator never gets msg2.
    /// Guards the IK static-key binding — the relay path's only authentication.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_static_key_is_rejected() {
        let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        // No device row seeded → the lookup-by-pubkey finds nothing.
        let device = StaticKeypair::generate().unwrap();
        let gw_static = load_or_create_static_keypair(&tg.deps.secret_vault)
            .await
            .expect("gateway static key");
        let gw_pub = gw_static.public();
        let state = WsChannelState::from_deps(&tg.deps);

        let (gw_tx, mut phone_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let (phone_tx, gw_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        tokio::spawn(async move {
            let _ = run_content_session(ChanSink(gw_tx), ChanSource(gw_rx), &state).await;
        });

        // msg1 carries the (unknown) static; the gateway aborts, so msg2 never comes.
        let mut hs: HandshakeState = device.ik_initiator(&gw_pub).unwrap();
        let mut buf = vec![0u8; NOISE_MAX_MESSAGE];
        let n = hs.write_message(&[], &mut buf).unwrap();
        phone_tx.send(buf[..n].to_vec()).await.unwrap();
        assert!(
            phone_rx.recv().await.is_none(),
            "gateway must not send msg2 for an unknown static key",
        );
    }
}
