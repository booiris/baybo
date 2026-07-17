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

use std::sync::Arc;
use std::time::Duration;

use baybo_channels::RouterInbound;
use baybo_channels::wire::{self, Frame, Message as WireMessage, MessageRole};
use baybo_model::ChannelType;
use baybo_pairing::DevicePairingService;
use baybo_store::{DeviceRow, DeviceStatus};
use device_proto::delegation;
use device_proto::noise::{FrameReassembler, NOISE_MAX_MESSAGE, StaticKeypair, write_chunked};
use device_proto::pairing::{
    ApnsEnv, DeviceConfirm, DeviceDelegation, DeviceHello, GatewayWelcome, PairFrame,
};
use device_proto::psk_pair::{PskHandshake, build_prologue};
use futures::{SinkExt, StreamExt};
use remote_host_admission::InMemoryAdmission;
use remote_host_protocol::relay::REMOTE_API_KEY_HEADER;
use remote_host_relay::serve::{IpLimitConfig, IpTrafficRegistry, RelayServices, build_router};
use remote_host_relay::{
    BandwidthRegistry, ConnectionRegistry, ControlRegistry, RelayBroker, TrafficRegistry,
};
use snow::TransportState;
use tokio_tungstenite::tungstenite::Message as TungMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use super::device_content::run_content_over_relay;
use super::device_pair::PairingHostDeps;
use super::relay_pair::host_pairing_leg;
use super::state::WsChannelState;
use crate::device::load_or_create_static_keypair;
use crate::test_support::build_test_deps;

/// A `tokio-tungstenite` client leg into the relay (plaintext `ws://` in-test).
type ClientWs = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const REMOTE_API_KEY: &str = "inst-A";
const NODE_ID: &str = "node-1";

/// Boot a real remote-host relay (C) on an ephemeral loopback port, admitting one
/// instance key. The per-IP limiter is off — irrelevant to the splice and it
/// would only see loopback. Returns the bound port.
async fn boot_relay() -> u16 {
    let admission = Arc::new(InMemoryAdmission::with_keys([REMOTE_API_KEY]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = build_router(
        RelayServices {
            admission,
            conns: Arc::new(ConnectionRegistry::new()),
            control: Arc::new(ControlRegistry::new()),
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

fn device_row(device_id: &str, pubkey: Vec<u8>) -> DeviceRow {
    DeviceRow {
        device_id: device_id.into(),
        device_pubkey: pubkey,
        auth_token: "device-auth-token-fixed-0123456789abcdef".into(),
        status: DeviceStatus::Approved,
        rendezvous_id: Some("11111111-2222-4333-8444-555555555555".into()),
        created_at: 0,
        approved_at: Some(0),
        last_seen_at: None,
        relay_url: "ws://relay.test".into(),
        remote_api_key: REMOTE_API_KEY.into(),
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
        .create(&device_row("device-dev", device.public().to_vec()))
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
    let host_ws = dial(port, &format!("/content/host/{relay_key}")).await;
    let responder_state = state.clone();
    tokio::spawn(async move {
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
                session_id: "sess-e2e".into(),
            },
        ),
    )
    .await;
    let msg = Frame::Message(WireMessage {
        content: "over real relay".into(),
        session_id: "sess-e2e".into(),
        user_id: "user-1".into(),
        channel_type: ChannelType::owner(),
        bot_id: String::new(),
        attachments: Vec::new(),
        platform_msg_id: "m1".into(),
        role: MessageRole::User,
        ordinal: None,
    });
    send_bin(&mut app, seal(&mut transport, &msg)).await;

    // The message reaches the gateway's router intake over the spliced path...
    let inbound = tokio::time::timeout(Duration::from_secs(5), incoming_rx.recv())
        .await
        .expect("router intake within timeout")
        .expect("router intake item");
    match inbound {
        RouterInbound::One(incoming) => {
            assert_eq!(incoming.message.session_id.as_str(), "sess-e2e");
        }
        other => panic!("expected RouterInbound::One, got {other:?}"),
    }

    // ...and the echo comes back to the app over the real spliced relay legs.
    let mut reassembler = FrameReassembler::new();
    let mut saw_echo = false;
    for _ in 0..16 {
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
        remote_api_key: REMOTE_API_KEY.into(),
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
        apns_token: "apns-tok".into(),
        apns_env: ApnsEnv::Sandbox,
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
    assert_eq!(row.auth_token, welcome.auth_token);
}
