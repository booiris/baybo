//! Cross-workspace end-to-end tests for the **spliced relay path** — both
//! pairing and content — through a **real** `remote-host` relay server (C).
//!
//! Every other relay test proves one side in isolation: the relay's own tests
//! splice raw bytes with no Noise, and the gateway's
//! `relay_path_resolves_device_by_pubkey_and_round_trips` /
//! `full_pairing_handshake_lands_approved_device` run the real Noise/XXpsk0
//! state machines over *in-memory* legs. Neither boots a real C, so the splice
//! itself was only ever proven on deployment.
//!
//! These close that gap. They boot a real `remote-host` relay (pulled in as a
//! dev-dependency across the workspace boundary) and drive the gateway's real
//! A-side machinery against a mock app over actual WebSockets, in one process:
//!
//! - **content** — a gateway control connection + the real Noise IK responder
//!   ([`run_content_over_relay`]) over a `/content/host` leg, a mock app over a
//!   `/content/join` leg; C signals + splices blind; a message round-trips.
//! - **pairing** — the real A-side pairing entry ([`host_pairing_leg`]) over a
//!   `/pair/host` leg, a mock XXpsk0 app over a `/pair/join` leg; the mutual
//!   confirm completes and an approved device row lands.
//! - **direct carriers** — the gateway's real relay-content manager holds an
//!   approved binding, its control connection and its carrier runtime against
//!   a C that also runs its UDP rendezvous. A mock phone seals an offer,
//!   posts it to C and gets A to admit it the ways P's tiers do: a `Peer`
//!   from C's rendezvous, an authenticated punch alone, or a host candidate
//!   in its offer. It then connects over QUIC or TCP with the pinned
//!   certificate, `DirectOpen` and Noise IK; a chat frame round-trips, and a
//!   revoke closes the carrier sessions. The crate builds with the
//!   protocol's `test-support`, so `AddressPolicy::for_tests` makes 127/8
//!   `Public` and `::1` `Lan`.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use baybo_agent::service::ShutdownSignal;
use baybo_channels::RouterInbound;
use baybo_channels::wire::{self, Frame, Message as WireMessage, MessageRole};
use baybo_model::ChannelType;
use baybo_pairing::DevicePairingService;
use baybo_store::{DeviceRow, DeviceStatus};
use carrier::kind::CarrierKind;
use carrier::rendezvous::{PEER_WAIT, RegisterOutcome, Registration};
use device_proto::aead::KEY_LEN;
use device_proto::candidates::{DeviceOffer, DeviceSealer, GatewayAnswer, OfferId};
use device_proto::delegation;
use device_proto::noise::{FrameReassembler, NOISE_MAX_MESSAGE, StaticKeypair, write_chunked};
use device_proto::pairing::{
    DeviceConfirm, DeviceDelegation, DeviceHello, GatewayWelcome, PairFrame,
};
use device_proto::psk_pair::{PskHandshake, build_prologue};
use futures::{SinkExt, StreamExt};
use remote_host_admission::InMemoryAdmission;
use remote_host_protocol::REMOTE_API_KEY_HEADER;
use remote_host_protocol::relay::{
    AddressPolicy, DirectOfferRequest, DirectOfferResponse, LegClass, ProbeDatagram, PunchId,
    PunchRole, SealedCandidates, UdpRendezvous, direct_offer_url,
};
use remote_host_relay::serve::{IpLimitConfig, IpTrafficRegistry, RelayServices, build_router};
use remote_host_relay::udp::{RendezvousAddress, RendezvousServer};
use remote_host_relay::{
    BandwidthRegistry, ConnectionRegistry, ControlRegistry, RelayBroker, TrafficRegistry,
};
use snow::TransportState;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as TungMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use super::carrier::offer::Decline;
use super::carrier::phone::{
    Phone, PhoneKeys, PhoneSession, STEP, close_code, ipv6_loopback_available, until,
};
use super::carrier::quic::CARRIER_REVOKED;
use super::device_content::run_content_over_relay;
use super::device_pair::PairingHostDeps;
use super::relay_content::{ControlTiming, run};
use super::relay_pair::host_pairing_leg;
use super::state::WsChannelState;
use crate::config::{FamilyBinds, RuntimeCarrierConfig, RuntimeDirectTcpConfig};
use crate::device::load_or_create_static_keypair;
use crate::relay::dial::RelayDialer;
use crate::relay::load_or_create_relay_node_id;
use crate::test_support::{TestGateway, build_test_deps};

/// A `tokio-tungstenite` client leg into the relay (plaintext `ws://` in-test).
type ClientWs = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const REMOTE_API_KEY: &str = "inst-A";
const NODE_ID: &str = "node-1";

/// Boot a real remote-host relay (C) on an ephemeral loopback port, admitting one
/// instance key. The per-IP limiter is off — irrelevant to the splice and it
/// would only see loopback. Returns the bound port.
async fn boot_relay() -> u16 {
    serve_relay(Arc::new(ControlRegistry::new())).await
}

/// [`boot_relay`] with `control` as C's control registry.
async fn serve_relay(control: Arc<ControlRegistry>) -> u16 {
    let admission = Arc::new(InMemoryAdmission::with_keys([REMOTE_API_KEY]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = build_router(
        RelayServices {
            admission,
            conns: Arc::new(ConnectionRegistry::new()),
            control,
            broker: Arc::new(RelayBroker::new()),
            bandwidth: Arc::new(BandwidthRegistry::new()),
            traffic: Arc::new(TrafficRegistry::new()),
            ip_traffic: Arc::new(IpTrafficRegistry::new()),
        },
        IpLimitConfig::disabled(),
    );
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    port
}

/// Dial a relay route, presenting the admitted instance key; panics on failure.
async fn dial(port: u16, path: &str) -> ClientWs {
    try_dial(port, path).await.expect("relay dial")
}

/// Dial a relay route, returning the error (e.g. a `503` before the gateway's
/// control connection has registered) so the caller can retry.
async fn try_dial(port: u16, path: &str) -> Result<ClientWs, String> {
    let url = format!("ws://127.0.0.1:{port}{path}");
    let mut req = url.into_client_request().map_err(|e| e.to_string())?;
    req.headers_mut()
        .insert(REMOTE_API_KEY_HEADER, REMOTE_API_KEY.parse().unwrap());
    connect_async(req)
        .await
        .map(|(ws, _)| ws)
        .map_err(|e| e.to_string())
}

async fn send_bin(ws: &mut ClientWs, bytes: Vec<u8>) {
    ws.send(TungMessage::Binary(bytes)).await.unwrap();
}

/// Next binary frame (ping/pong skipped); `None` on close.
async fn recv_bin(ws: &mut ClientWs) -> Option<Vec<u8>> {
    loop {
        match ws.next().await {
            Some(Ok(TungMessage::Binary(b))) => return Some(b),
            Some(Ok(TungMessage::Ping(_) | TungMessage::Pong(_))) => continue,
            _ => return None,
        }
    }
}

async fn recv_json(ws: &mut ClientWs) -> serde_json::Value {
    let b = recv_bin(ws).await.expect("control signal");
    serde_json::from_slice(&b).unwrap()
}

/// Send one `PairFrame` (msgpack) over a relay leg, the way the app does.
async fn send_pair_frame(ws: &mut ClientWs, frame: &PairFrame) {
    let bytes = device_proto::pairing::encode(frame).unwrap();
    ws.send(TungMessage::Binary(bytes)).await.unwrap();
}

/// Next `PairFrame` over a relay leg (ping/pong skipped).
async fn recv_pair_frame(ws: &mut ClientWs) -> PairFrame {
    loop {
        match ws.next().await {
            Some(Ok(TungMessage::Binary(b))) => {
                return device_proto::pairing::decode(&b).unwrap();
            }
            Some(Ok(TungMessage::Ping(_) | TungMessage::Pong(_))) => continue,
            other => panic!("unexpected pairing frame: {other:?}"),
        }
    }
}

/// Seal one frame the way the app does — encode then chunk under the transport.
/// Test frames fit a single chunk.
fn seal(transport: &mut TransportState, frame: &Frame) -> Vec<u8> {
    let plaintext = wire::encode(frame).unwrap();
    let mut messages = write_chunked(transport, &plaintext).unwrap();
    assert_eq!(messages.len(), 1, "test frames fit one chunk");
    messages.remove(0)
}

fn device_row(device_id: &str, pubkey: Vec<u8>, relay_url: &str) -> DeviceRow {
    DeviceRow {
        device_id: device_id.into(),
        device_pubkey: pubkey,
        auth_token_sha256: baybo_store::device::hash_auth_token(
            "device-auth-token-fixed-0123456789abcdef",
        ),
        status: DeviceStatus::Approved,
        rendezvous_id: Some("11111111-2222-4333-8444-555555555555".into()),
        created_at: 0,
        approved_at: Some(0),
        last_seen_at: None,
        relay_url: relay_url.into(),
        push_url: "https://push.test".into(),
        remote_api_key: REMOTE_API_KEY.into(),
    }
}

/// The chat session every E2E message is sent in.
const CHAT_SESSION: &str = "sess-e2e";
/// How many frames the app reads while waiting for the echo of its message.
const MAX_FRAMES_BEFORE_ECHO: usize = 16;

/// A user message in [`CHAT_SESSION`], as the app sends one.
fn user_message(content: &str) -> Frame {
    Frame::Message(WireMessage {
        content: content.into(),
        session_id: CHAT_SESSION.into(),
        user_id: "user-1".into(),
        channel_type: ChannelType::owner(),
        bot_id: String::new(),
        attachments: Vec::new(),
        platform_msg_id: "m1".into(),
        role: MessageRole::User,
        ordinal: None,
    })
}

/// Asserts the app's message reached the gateway's router intake.
async fn expect_router_intake(incoming_rx: &mut mpsc::Receiver<RouterInbound>) {
    let inbound = tokio::time::timeout(Duration::from_secs(5), incoming_rx.recv())
        .await
        .expect("router intake within timeout")
        .expect("router intake item");
    match inbound {
        RouterInbound::One(incoming) => {
            assert_eq!(incoming.message.session_id.as_str(), CHAT_SESSION);
        }
        other => panic!("expected RouterInbound::One, got {other:?}"),
    }
}

/// Full spliced path through a real C: the gateway's Noise responder and a mock
/// app meet over actual `/content/host` and `/content/join` legs, C blindly
/// splicing the two. A user message reaches the gateway's router intake and the
/// echo comes back to the app — all over real WebSockets.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_relay_splices_gateway_responder_and_mock_app() {
    let port = boot_relay().await;

    // Gateway deps: an approved device (keyed by the mock app's static), the device
    // channel, and the gateway's own static key the app handshakes against.
    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let device = StaticKeypair::generate().unwrap();
    tg.deps
        .stores
        .device
        .create(&device_row(
            "device-dev",
            device.public().to_vec(),
            "ws://relay.test",
        ))
        .await
        .expect("seed approved device row");
    // Device connections pool into the shared `owner` channel.
    crate::channel::boot::install_channel(&tg.deps.channel_registry, ChannelType::owner())
        .expect("install owner channel");
    let gw_static = load_or_create_static_keypair(&tg.deps.secret_vault)
        .await
        .expect("gateway static key");
    let gw_pub = gw_static.public();
    let state = WsChannelState::from_deps(&tg.deps);
    let mut incoming_rx = tg.incoming_rx;

    // The gateway holds its control connection under NODE_ID.
    let mut control = dial(port, "/control").await;
    send_bin(
        &mut control,
        serde_json::to_vec(&serde_json::json!({ "relay_node_id": NODE_ID })).unwrap(),
    )
    .await;

    // The mock app dials content/join; control registration is async, so retry
    // until C admits the join (the gateway is registered and was signalled).
    let mut app = None;
    for _ in 0..80 {
        match try_dial(port, &format!("/content/join/{NODE_ID}")).await {
            Ok(ws) => {
                app = Some(ws);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
        }
    }
    let mut app = app.expect("content/join admitted once the gateway control registers");

    // C signalled the gateway to open a data leg under a fresh relay_key.
    let signal = recv_json(&mut control).await;
    assert_eq!(signal["t"], "open_data_leg");
    let relay_key = signal["relay_key"].as_str().unwrap().to_owned();

    // The gateway opens its host leg and runs the real Noise IK responder over it.
    let host_ws = RelayDialer::direct()
        .dial(
            &format!("ws://127.0.0.1:{port}/content/host/{relay_key}"),
            REMOTE_API_KEY,
        )
        .await
        .expect("gateway content host leg via the production dialer");
    let responder_state = state.clone();
    let responder = tokio::spawn(async move {
        run_content_over_relay(host_ws, &responder_state, None).await;
    });

    // The mock app: IK initiator handshake over the spliced relay legs.
    let mut hs = device.ik_initiator(&gw_pub).unwrap();
    let mut buf = vec![0u8; NOISE_MAX_MESSAGE];
    let n = hs.write_message(&[], &mut buf).unwrap();
    send_bin(&mut app, buf[..n].to_vec()).await;
    let msg2 = recv_bin(&mut app)
        .await
        .expect("gateway handshake msg2 over the relay");
    hs.read_message(&msg2, &mut buf).unwrap();
    let mut transport = hs.into_transport_mode().unwrap();

    // Subscribe + a user message, sealed and chunked exactly as the app does.
    send_bin(
        &mut app,
        seal(
            &mut transport,
            &Frame::Subscribe {
                session_id: CHAT_SESSION.into(),
            },
        ),
    )
    .await;
    send_bin(
        &mut app,
        seal(&mut transport, &user_message("over real relay")),
    )
    .await;

    // The message reaches the gateway's router intake over the spliced path...
    expect_router_intake(&mut incoming_rx).await;

    // ...and the echo comes back to the app over the real spliced relay legs.
    let mut reassembler = FrameReassembler::new();
    let mut saw_echo = false;
    for _ in 0..MAX_FRAMES_BEFORE_ECHO {
        let Some(bytes) = recv_bin(&mut app).await else {
            break;
        };
        for frame in reassembler.read(&mut transport, &bytes).unwrap() {
            if let Frame::Message(m) = wire::decode(&frame).unwrap() {
                assert_eq!(m.content, "over real relay");
                saw_echo = true;
            }
        }
        if saw_echo {
            break;
        }
    }
    assert!(
        saw_echo,
        "the mock app never received the echo over the real relay"
    );

    // The authenticated relay leg is in the link table until it ends.
    let links = state.device_links.snapshot();
    let [record] = links.as_slice() else {
        panic!("one linked device: {links:?}");
    };
    assert_eq!(record.device_id, "device-dev");
    let legs: Vec<_> = record
        .legs
        .iter()
        .map(|leg| (leg.class, leg.kind))
        .collect();
    assert_eq!(legs, [(LegClass::Chat, Some(CarrierKind::Relay))]);
    drop(app);
    tokio::time::timeout(Duration::from_secs(5), responder)
        .await
        .expect("the relay leg ends once the app leaves")
        .expect("the responder task");
    assert!(
        state.device_links.snapshot().is_empty(),
        "an ended relay leg leaves the table"
    );
}

/// Full **pairing** path through a real C: the gateway's real A-side entry
/// ([`host_pairing_leg`]) hosts `/pair/host`, a mock XXpsk0 app joins `/pair/join`,
/// C splices them blind, and the mutual-confirm handshake completes — landing an
/// approved device row with an active auth token. Proves the spliced *pairing*
/// path end to end across the workspace boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_relay_pairs_gateway_and_mock_app() {
    let port = boot_relay().await;
    let relay_url = format!("ws://127.0.0.1:{port}");

    let tg = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
    let device_store = tg.deps.stores.device.clone();
    let device_pairing = Arc::new(DevicePairingService::new(device_store.clone()));

    // Mint a rendezvous + its QR secret; the operator confirms up front (no race).
    let (rid, secret) = device_pairing.mint().await.unwrap();
    device_pairing
        .set_operator_decision(&rid, true)
        .await
        .unwrap();

    // The gateway's real A-side: dial `/pair/host`, run XXpsk0 + mutual confirm.
    // The prologue binds `deps.relay_url`, so the app must use the same endpoint.
    let deps = PairingHostDeps {
        device_pairing: Arc::clone(&device_pairing),
        secret_vault: tg.deps.secret_vault.clone(),
        relay_url: relay_url.clone(),
        push_url: "https://push.test".into(),
        remote_api_key: REMOTE_API_KEY.into(),
        relay_dialer: RelayDialer::direct(),
    };
    let gateway = {
        let relay_url = relay_url.clone();
        let rid = rid.clone();
        tokio::spawn(async move { host_pairing_leg(&deps, &relay_url, REMOTE_API_KEY, &rid).await })
    };

    // Mock app dials `/pair/join`; the join uses try-match (never parks), so retry
    // until the gateway's host leg is parked on the relay.
    let mut app = None;
    for _ in 0..80 {
        match try_dial(port, &format!("/pair/join/{rid}")).await {
            Ok(ws) => {
                app = Some(ws);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(25)).await,
        }
    }
    let mut app = app.expect("/pair/join matches the parked host leg");

    // XXpsk0 initiator: Hello (e) → HandshakeReply → HandshakeFinal(DeviceHello).
    let app_static = StaticKeypair::generate().unwrap();
    let prologue = build_prologue(&rid, &relay_url);
    let mut hs = PskHandshake::start_initiator(&app_static.secret(), &secret, &prologue).unwrap();
    let msg1 = hs.write_handshake(&[]).unwrap();
    send_pair_frame(
        &mut app,
        &PairFrame::Hello {
            rendezvous_id: rid.clone(),
            msg: msg1,
        },
    )
    .await;
    let PairFrame::HandshakeReply { msg } = recv_pair_frame(&mut app).await else {
        panic!("expected HandshakeReply");
    };
    hs.read_handshake(&msg).unwrap();
    // The app's Ed25519 identity; its public half is the device_id.
    let app_ed = delegation::generate_signing_key();
    let device_id = delegation::device_id_for(&app_ed.verifying_key());
    let hello = DeviceHello {
        device_id: device_id.clone(),
    };
    let msg3 = hs
        .write_handshake(&device_proto::pairing::encode(&hello).unwrap())
        .unwrap();
    send_pair_frame(&mut app, &PairFrame::HandshakeFinal { msg: msg3 }).await;
    let mut transport = hs.into_transport().unwrap();

    // Phone confirms → sealed DeviceConfirm; the gateway then seals GatewayWelcome.
    let confirm = transport
        .write(&device_proto::pairing::encode(&DeviceConfirm { accepted: true }).unwrap())
        .unwrap();
    send_pair_frame(&mut app, &PairFrame::Sealed { msg: confirm }).await;

    let PairFrame::Sealed { msg } = recv_pair_frame(&mut app).await else {
        panic!("expected sealed GatewayWelcome");
    };
    let welcome: GatewayWelcome =
        device_proto::pairing::decode(&transport.read(&msg).unwrap()).unwrap();
    assert!(
        !welcome.auth_token.is_empty(),
        "welcome carries an active token"
    );
    assert_eq!(welcome.rendezvous_id, rid);

    // 6th message: the device delegates the gateway push key (signed under the
    // device identity whose public half is the device_id). Without it the
    // gateway's host leg waits out the full delegation timeout before completing.
    let gw_push = delegation::verifying_key_from_bytes(&welcome.gateway_push_pubkey).unwrap();
    let deleg = delegation::sign_delegation(&app_ed, &gw_push);
    let deleg_frame = transport
        .write(
            &device_proto::pairing::encode(&DeviceDelegation {
                delegation: deleg.to_bytes().to_vec(),
            })
            .unwrap(),
        )
        .unwrap();
    send_pair_frame(&mut app, &PairFrame::Sealed { msg: deleg_frame }).await;

    gateway
        .await
        .expect("gateway pairing task joins")
        .expect("gateway pairing leg completes");

    // An approved device row landed over the real relay pairing splice.
    let row = device_store.get(&device_id).await.unwrap().unwrap();
    assert_eq!(row.status, DeviceStatus::Approved);
    assert_eq!(
        row.auth_token_sha256,
        baybo_store::device::hash_auth_token(&welcome.auth_token),
        "the row stores the digest of the bearer the device was handed"
    );
}

const LOOPBACK_V4: &str = "127.0.0.1:0";
const LOOPBACK_V6: &str = "[::1]:0";

/// A real C with its IPv4 UDP rendezvous on loopback beside its routes.
struct DirectRelay {
    port: u16,
    control: Arc<ControlRegistry>,
}

impl DirectRelay {
    async fn boot() -> Self {
        let rendezvous = RendezvousServer::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = RendezvousAddress::parse(&rendezvous.local_addr().unwrap().to_string())
            .expect("a loopback rendezvous is Public under the test policy");
        let control = Arc::new(ControlRegistry::new().with_udp_rendezvous(address));
        tokio::spawn(rendezvous.serve(Arc::clone(&control)));
        let port = serve_relay(Arc::clone(&control)).await;
        Self { port, control }
    }

    fn url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.port)
    }
}

/// A's answer to an accepted offer, opened, and what C handed P beside it.
struct Answered {
    punch_id: PunchId,
    offer_id: OfferId,
    answer: GatewayAnswer,
    rendezvous: Option<UdpRendezvous>,
}

/// A gateway whose real relay-content manager holds an approved binding, and
/// its carrier runtime, against a real C; and the paired phone's keys.
struct DirectRig {
    relay: DirectRelay,
    gateway: TestGateway,
    state: WsChannelState,
    device_id: String,
    device: StaticKeypair,
    gateway_public: [u8; KEY_LEN],
    relay_node_id: String,
    shutdown: ShutdownSignal,
    manager: JoinHandle<()>,
}

impl DirectRig {
    /// Starts the manager with `carrier` and waits until its control
    /// connection is registered at C, with the runtime's capability.
    async fn start(carrier: RuntimeCarrierConfig) -> Self {
        let relay = DirectRelay::boot().await;
        let gateway = build_test_deps("127.0.0.1:0".parse().unwrap()).await;
        crate::channel::boot::install_channel(&gateway.deps.channel_registry, ChannelType::owner())
            .expect("install owner channel");
        let device = StaticKeypair::generate().unwrap();
        let device_id =
            delegation::device_id_for(&delegation::generate_signing_key().verifying_key());
        gateway
            .deps
            .stores
            .device
            .create(&device_row(
                &device_id,
                device.public().to_vec(),
                &relay.url(),
            ))
            .await
            .expect("seed the approved device");
        let gateway_public = load_or_create_static_keypair(&gateway.deps.secret_vault)
            .await
            .unwrap()
            .public();
        let relay_node_id = load_or_create_relay_node_id(&gateway.deps.secret_vault)
            .await
            .unwrap();
        let state = WsChannelState::from_deps(&gateway.deps);
        let shutdown = ShutdownSignal::new();
        let manager = tokio::spawn(run(
            state.clone(),
            carrier,
            shutdown.clone(),
            ControlTiming::FAST,
        ));
        until("the gateway's control connection registers at C", || {
            relay.control.connected() == 1
        })
        .await;
        Self {
            relay,
            gateway,
            state,
            device_id,
            device,
            gateway_public,
            relay_node_id,
            shutdown,
            manager,
        }
    }

    fn keys(&self) -> PhoneKeys<'_> {
        PhoneKeys {
            device: &self.device,
            gateway_public: &self.gateway_public,
        }
    }

    fn sealer(&self) -> DeviceSealer {
        DeviceSealer::derive(&self.device.secret(), &self.gateway_public).unwrap()
    }

    /// An offer of `udp`, issued now and sealed for this gateway, and its id.
    fn seal_offer(&self, udp: Vec<SocketAddr>) -> (SealedCandidates, OfferId) {
        let issued_at_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap();
        let offer = DeviceOffer::new(issued_at_ms, udp);
        let sealed = self
            .sealer()
            .seal_offer(&self.relay_node_id, &offer)
            .unwrap();
        (sealed, offer.offer_id)
    }

    /// `POST /direct/{relay_node_id}` to C, as P sends it.
    async fn post(&self, offer: SealedCandidates) -> reqwest::Response {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(STEP)
            .build()
            .unwrap()
            .post(direct_offer_url(&self.relay.url(), &self.relay_node_id))
            .header(REMOTE_API_KEY_HEADER, REMOTE_API_KEY)
            .json(&DirectOfferRequest { offer })
            .send()
            .await
            .expect("C answers the offer")
    }

    /// Offers `udp` through C and opens A's answer.
    async fn offer(&self, udp: Vec<SocketAddr>) -> Answered {
        let (sealed, offer_id) = self.seal_offer(udp);
        let response = self.post(sealed).await;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let response: DirectOfferResponse = response.json().await.unwrap();
        let answer = self
            .sealer()
            .open_answer(
                &self.relay_node_id,
                &response.punch_id,
                &offer_id,
                &response.answer,
            )
            .expect("A's answer opens and echoes the offer id");
        Answered {
            punch_id: response.punch_id,
            offer_id,
            answer,
            rendezvous: response.rendezvous,
        }
    }

    /// Subscribes and sends a user message over `session`, then waits for
    /// the gateway to take it in and echo it back.
    async fn chat_round_trip<W, R>(&mut self, session: &mut PhoneSession<W, R>, content: &str)
    where
        W: AsyncWrite + Unpin,
        R: AsyncRead + Unpin,
    {
        let subscribe = Frame::Subscribe {
            session_id: CHAT_SESSION.into(),
        };
        session
            .send_plaintext(&wire::encode(&subscribe).unwrap())
            .await;
        session
            .send_plaintext(&wire::encode(&user_message(content)).unwrap())
            .await;
        expect_router_intake(&mut self.gateway.incoming_rx).await;
        for _ in 0..MAX_FRAMES_BEFORE_ECHO {
            if let Frame::Message(echo) = wire::decode(&session.recv_plaintext().await).unwrap() {
                assert_eq!(echo.content, content);
                return;
            }
        }
        panic!("the phone never received the echo over the carrier");
    }

    /// The paired device's legs in the link table, and its last offer's
    /// outcome.
    fn listed(&self) -> (Vec<ListedLeg>, Option<Result<(), Decline>>) {
        self.state
            .device_links
            .snapshot()
            .into_iter()
            .find(|record| record.device_id == self.device_id)
            .map(|record| {
                (
                    record
                        .legs
                        .iter()
                        .map(|leg| (leg.class, leg.kind))
                        .collect(),
                    record.last_offer.map(|offer| offer.outcome),
                )
            })
            .unwrap_or_default()
    }

    async fn stop(self) {
        self.shutdown.trigger();
        tokio::time::timeout(STEP, self.manager)
            .await
            .expect("the manager returns on shutdown")
            .expect("the manager task did not panic");
    }
}

type ListedLeg = (LegClass, Option<CarrierKind>);

fn udp_carriers(ipv4: Option<&str>, ipv6: Option<&str>) -> RuntimeCarrierConfig {
    RuntimeCarrierConfig {
        udp: Some(FamilyBinds {
            ipv4: ipv4.map(|bind| bind.parse().unwrap()),
            ipv6: ipv6.map(|bind| bind.parse().unwrap()),
        }),
        tcp: None,
    }
}

/// A TCP listener on loopback and, when `udp`, an IPv4 UDP socket too.
fn tcp_carriers(udp: bool) -> RuntimeCarrierConfig {
    RuntimeCarrierConfig {
        udp: udp.then(|| FamilyBinds {
            ipv4: Some(LOOPBACK_V4.parse().unwrap()),
            ipv6: None,
        }),
        tcp: Some(RuntimeDirectTcpConfig {
            binds: FamilyBinds {
                ipv4: Some(LOOPBACK_V4.parse().unwrap()),
                ipv6: None,
            },
            advertised_addresses: Vec::new(),
        }),
    }
}

/// P's registration at C's rendezvous for one punch, from `phone`'s socket;
/// returns A's mapping from C's `Peer`.
async fn register_device(
    phone: &mut Phone,
    punch_id: PunchId,
    rendezvous: UdpRendezvous,
) -> SocketAddrV4 {
    let policy = AddressPolicy::active();
    let ticket = rendezvous.ticket.clone();
    let address = tokio::task::spawn_blocking(move || rendezvous.resolve_public_v4(&policy))
        .await
        .unwrap()
        .expect("C's rendezvous address resolves to Public IPv4");
    let mut registration = Registration::new(punch_id, PunchRole::Device, ticket, address, policy);
    let deadline = tokio::time::Instant::now() + PEER_WAIT;
    match registration
        .until_peer(&phone.socket, &mut phone.probes, deadline)
        .await
    {
        RegisterOutcome::Peer(gateway) => gateway,
        RegisterOutcome::NoPeer { registered } => {
            panic!("C returned no Peer (registered: {registered})")
        }
    }
}

/// Sends P's authenticated punch of `offer_id` to `target`.
async fn punch(phone: &Phone, sealer: &DeviceSealer, offer_id: &OfferId, target: SocketAddr) {
    let punch = ProbeDatagram::Punch {
        seq: 0,
        tag: sealer.punch_tag(offer_id, 0).unwrap(),
    };
    phone.socket.send_probe(&punch, target, None).await.unwrap();
}

/// Waits for a punch from `from`. A punches a `Peer` mapping only once it is
/// in the punch's allowed set, so the phone may then dial A.
async fn next_punch_from(phone: &mut Phone, from: SocketAddr) {
    tokio::time::timeout(STEP, async {
        while let Some(probe) = phone.probes.recv().await {
            if probe.source == from && matches!(probe.datagram, ProbeDatagram::Punch { .. }) {
                return;
            }
        }
        panic!("the phone's probe queue closed");
    })
    .await
    .expect("the gateway punches the phone's mapping");
}

/// The rendezvous path P's `ipv4_punched` tier takes, over 127.0.0.1: the
/// phone registers at C's UDP rendezvous, learns A's mapping from `Peer`,
/// waits for A's punch and connects there over QUIC; a chat frame
/// round-trips. The phone offers no host candidate and, since loopback has
/// no NAT to open, sends no punch of its own, so C's `Peer` is the only
/// thing that can admit it: A allows the mapping it latches before it
/// punches it. Loopback's mapping is A's host address, which is `Public`
/// under the test policy: A lists the leg as `ipv4`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_relay_rendezvous_punches_a_quic_carrier_over_ipv4_loopback() {
    let mut rig = DirectRig::start(udp_carriers(Some(LOOPBACK_V4), None)).await;
    let mut phone = Phone::bind(LOOPBACK_V4.parse().unwrap());

    let answered = rig.offer(Vec::new()).await;
    let rendezvous = answered
        .rendezvous
        .expect("C runs a rendezvous for a gateway with IPv4 UDP");
    let mapping = register_device(&mut phone, answered.punch_id, rendezvous).await;
    let gateway = SocketAddr::V4(mapping);
    assert_eq!(
        answered.answer.udp,
        [gateway],
        "on loopback A's mapping is its host address"
    );
    next_punch_from(&mut phone, gateway).await;

    let connection = phone
        .connect(answered.answer.quic_cert_sha256, gateway)
        .await;
    let mut chat = rig
        .keys()
        .session(&connection, &answered.answer.token, LegClass::Chat)
        .await;
    rig.chat_round_trip(&mut chat, "over a punched carrier")
        .await;
    assert_eq!(
        rig.listed(),
        (
            vec![(LegClass::Chat, Some(CarrierKind::Ipv4))],
            Some(Ok(()))
        )
    );
    rig.stop().await;
}

/// P dials A's ULA-class host candidate, `::1` under the test policy, after
/// an authenticated punch; a chat frame round-trips over the `Lan` carrier.
/// A without an IPv4 UDP family asks C for no rendezvous, and the phone
/// offers no host candidate, so its authenticated punch alone admits it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_relay_offer_opens_a_lan_carrier_over_ipv6_loopback() {
    if !ipv6_loopback_available() {
        eprintln!("skipping: no IPv6 loopback on this host");
        return;
    }
    let mut rig = DirectRig::start(udp_carriers(None, Some(LOOPBACK_V6))).await;
    let phone = Phone::bind(LOOPBACK_V6.parse().unwrap());

    let answered = rig.offer(Vec::new()).await;
    assert!(answered.rendezvous.is_none(), "no IPv4 UDP, no rendezvous");
    let gateway = *answered
        .answer
        .udp
        .iter()
        .find(|candidate| candidate.ip() == Ipv6Addr::LOCALHOST)
        .expect("A offers its ::1 host candidate");
    punch(&phone, &rig.sealer(), &answered.offer_id, gateway).await;

    // A may judge the phone's first Initial before the punch it received
    // earlier and ignore it; quinn's retransmitted Initial then gets in.
    let connection = phone
        .connect(answered.answer.quic_cert_sha256, gateway)
        .await;
    let mut chat = rig
        .keys()
        .session(&connection, &answered.answer.token, LegClass::Chat)
        .await;
    rig.chat_round_trip(&mut chat, "over a lan carrier").await;
    assert_eq!(rig.listed().0, [(LegClass::Chat, Some(CarrierKind::Lan))]);
    rig.stop().await;
}

/// With `direct_tcp` configured, A's answer carries its listener, and a chat
/// frame round-trips over a TCP leg.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_relay_offer_opens_a_tcp_carrier_when_tcp_is_configured() {
    let mut rig = DirectRig::start(tcp_carriers(false)).await;

    let answered = rig.offer(Vec::new()).await;
    let [listener] = answered.answer.tcp[..] else {
        panic!("one TCP candidate: {:?}", answered.answer.tcp);
    };
    let mut chat = rig
        .keys()
        .tcp_session(listener, &answered.answer.token, LegClass::Chat)
        .await;
    rig.chat_round_trip(&mut chat, "over a tcp carrier").await;
    assert_eq!(rig.listed().0, [(LegClass::Chat, Some(CarrierKind::Tcp))]);
    rig.stop().await;
}

/// Revoking the device mid-session ends the binding scope: its QUIC
/// connection closes with `CARRIER_REVOKED`, its TCP session ends, the legs
/// leave the link table, and C no longer routes the phone's offers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoking_the_device_closes_its_live_carrier_sessions() {
    let mut rig = DirectRig::start(tcp_carriers(true)).await;
    let phone = Phone::bind(LOOPBACK_V4.parse().unwrap());
    let answered = rig.offer(vec![phone.address()]).await;
    let token = &answered.answer.token;

    let connection = phone
        .connect(answered.answer.quic_cert_sha256, answered.answer.udp[0])
        .await;
    let mut chat = rig.keys().session(&connection, token, LegClass::Chat).await;
    rig.chat_round_trip(&mut chat, "before the revoke").await;
    let mut tunnel = rig
        .keys()
        .tcp_session(answered.answer.tcp[0], token, LegClass::Api)
        .await;
    until("both carrier legs are listed", || rig.listed().0.len() == 2).await;

    rig.gateway
        .deps
        .stores
        .device
        .revoke(&rig.device_id)
        .await
        .unwrap();
    assert_eq!(close_code(&connection).await, CARRIER_REVOKED);
    chat.ended().await;
    tunnel.ended().await;
    until("the revoked binding's legs leave the table", || {
        rig.listed().0.is_empty()
    })
    .await;
    until("C drops the ended binding's control connection", || {
        rig.relay.control.connected() == 0
    })
    .await;
    let (sealed, _) = rig.seal_offer(vec![phone.address()]);
    assert_eq!(
        rig.post(sealed).await.status(),
        reqwest::StatusCode::NOT_FOUND
    );
    rig.stop().await;
}

/// An offer tampered with on the way is declined by A, which C answers with
/// its opaque `404`; the route stays up for the next genuine offer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tampered_offer_is_declined_and_c_answers_404() {
    let rig = DirectRig::start(udp_carriers(Some(LOOPBACK_V4), None)).await;
    let (mut sealed, _) = rig.seal_offer(Vec::new());
    let mut ciphertext = STANDARD.decode(&sealed.enc).unwrap();
    *ciphertext.last_mut().unwrap() ^= 1;
    sealed.enc = STANDARD.encode(ciphertext);

    assert_eq!(
        rig.post(sealed).await.status(),
        reqwest::StatusCode::NOT_FOUND
    );
    assert_eq!(
        rig.listed().1,
        Some(Err(Decline::Auth)),
        "the 404 is A's decline, not a missing route"
    );
    rig.offer(Vec::new()).await;
    assert_eq!(rig.listed().1, Some(Ok(())));
    rig.stop().await;
}
