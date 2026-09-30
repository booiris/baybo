use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket as StdUdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use baybo_channels::wire::{self, Frame};
use baybo_model::BlobRef;
use baybo_storage::test_support::MemoryBlobStore;
use baybo_store::blob::{BlobMeta, BlobReader, BlobStore, ByteStream, Result as BlobResult};
use baybo_store::{DeviceRow, DeviceStatus, StorageError};
use carrier::burst::PUNCH_BURST;
use carrier::framing::{FrameReader, write_direct_open, write_frame};
use carrier::kind::CarrierKind;
use carrier::quic::{DIRECT_QUIC_SERVER_NAME, client_config};
use device_proto::api_tunnel::{TunnelRequest, TunnelResponse};
use device_proto::candidates::{DeviceSealer, OfferId};
use device_proto::delegation::{device_id_for, generate_signing_key};
use device_proto::noise::{NOISE_MAX_MESSAGE, StaticKeypair, write_chunked};
use futures::StreamExt;
use remote_host_protocol::relay::{
    DirectOpen, LegClass, PUNCH_TAG_LEN, ProbeDatagram, PunchRole, PunchTag, RENDEZVOUS_TICKET_LEN,
    RendezvousTicket,
};
use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::*;
use crate::channel::device_content::{BinarySink, BinarySource, run_content_session};
use crate::channel::state::LegDedup;
use crate::config::FamilyBinds;
use crate::device::load_or_create_static_keypair;
use crate::test_support::{TestGateway, build_test_deps};

use super::super::offer::{MAX_REPLAY_ENTRIES, OFFER_MAX_AGE};
use super::super::phone::{
    Phone, PhoneKeys, QUIET, QuicSession, STEP, close_code, ipv6_loopback_available, until,
};
use super::super::quic::{CARRIER_UNAUTHENTICATED, MAX_QUIC_CONNECTIONS_PER_SOURCE};
use super::super::udp::REBIND_AFTER_RECV_ERRORS;

const NODE_ID: &str = "node-1";
const LOOPBACK: &str = "127.0.0.1:0";
const WILDCARD_V4: &str = "0.0.0.0:0";
/// A host the tests never reach: `AddressPolicy::for_tests` classes 198.18/15
/// as `Public`, and A's loopback socket cannot send to it.
const UNREACHED_HOST: &str = "198.18.0.1";
const SHORT_DEADLINE: Duration = Duration::from_millis(400);
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(1);
const STALLED_PREFACE_DEADLINE: Duration = Duration::from_secs(30);
const UDP_BUFFER_LEN: usize = 2048;
const TEST_REBIND_DELAY: Duration = Duration::from_millis(50);
const BLOB_BODY_LEN: u64 = 1024;

fn udp_only(ipv4: Option<&str>, ipv6: Option<&str>) -> RuntimeCarrierConfig {
    RuntimeCarrierConfig {
        udp: Some(FamilyBinds {
            ipv4: ipv4.map(|bind| bind.parse().unwrap()),
            ipv6: ipv6.map(|bind| bind.parse().unwrap()),
        }),
    }
}

fn loopback() -> RuntimeCarrierConfig {
    udp_only(Some(LOOPBACK), None)
}

fn socket(address: &str) -> SocketAddr {
    address.parse().unwrap()
}

fn host(port: u16) -> SocketAddr {
    SocketAddr::new(UNREACHED_HOST.parse().unwrap(), port)
}

fn port_is_held(address: SocketAddr) -> bool {
    match StdUdpSocket::bind(address) {
        Ok(_) => false,
        Err(error) if error.kind() == ErrorKind::AddrInUse => true,
        Err(error) => panic!("probe bind of {address}: {error}"),
    }
}

fn active(runtime: &CarrierRuntime) -> &ActiveRuntime {
    runtime.active.as_ref().expect("an active runtime")
}

fn context(runtime: &CarrierRuntime) -> &RuntimeContext {
    &active(runtime).context
}

fn udp_address(runtime: &CarrierRuntime) -> SocketAddr {
    active(runtime).bound_addresses()[0]
}

fn bound_socket(runtime: &CarrierRuntime) -> BoundSocket {
    active(runtime).udp[0].current().expect("a bound family")
}

fn admits(runtime: &CarrierRuntime, ip: &str) -> bool {
    context(runtime)
        .punches
        .lock()
        .admits(ip.parse().unwrap(), Instant::now())
}

fn device_row(device_id: &str, pubkey: Vec<u8>) -> DeviceRow {
    DeviceRow {
        device_id: device_id.to_owned(),
        device_pubkey: pubkey,
        auth_token_sha256: baybo_store::device::hash_auth_token(
            "device-auth-token-fixed-0123456789abcdef",
        ),
        status: DeviceStatus::Approved,
        rendezvous_id: None,
        created_at: 0,
        approved_at: Some(0),
        last_seen_at: None,
        relay_url: "ws://relay.test".to_owned(),
        push_url: "https://push.test".to_owned(),
        remote_api_key: "inst-A".to_owned(),
    }
}

/// A gateway with one approved device, and that device's static key.
struct Pairing {
    _gateway: TestGateway,
    state: WsChannelState,
    /// Derived from an ed25519 identity, as on a real device: the tunnel's
    /// auth middleware refuses a made-up id before any route runs.
    device_id: String,
    device: StaticKeypair,
    gateway_public: [u8; KEY_LEN],
}

impl Pairing {
    async fn new() -> Self {
        let gateway = build_test_deps(LOOPBACK.parse().unwrap()).await;
        let device = StaticKeypair::generate().unwrap();
        let device_id = device_id_for(&generate_signing_key().verifying_key());
        gateway
            .deps
            .stores
            .device
            .create(&device_row(&device_id, device.public().to_vec()))
            .await
            .unwrap();
        let gateway_public = load_or_create_static_keypair(&gateway.deps.secret_vault)
            .await
            .unwrap()
            .public();
        let state = WsChannelState::from_deps(&gateway.deps);
        Self {
            _gateway: gateway,
            state,
            device_id,
            device,
            gateway_public,
        }
    }

    /// The same pairing, whose gateway stores uploads in `blobs`.
    fn storing_blobs_in(mut self, blobs: Arc<dyn BlobStore>) -> Self {
        self.state.blob_store = blobs;
        self
    }

    async fn binding(&self, timing: CarrierTiming) -> CarrierBinding {
        let mut binding =
            CarrierBinding::derive(&self.state, NODE_ID, &self.device_id, &self.device.public())
                .await
                .unwrap();
        binding.timing = timing;
        binding
    }

    async fn runtime(&self, config: &RuntimeCarrierConfig) -> CarrierRuntime {
        self.runtime_with(
            config,
            &mut CarrierProcess::new(),
            CarrierTiming::PRODUCTION,
        )
        .await
    }

    async fn runtime_with(
        &self,
        config: &RuntimeCarrierConfig,
        process: &mut CarrierProcess,
        timing: CarrierTiming,
    ) -> CarrierRuntime {
        CarrierRuntime::bind(config, process, self.binding(timing).await)
    }

    fn sealer(&self) -> DeviceSealer {
        DeviceSealer::derive(&self.device.secret(), &self.gateway_public).unwrap()
    }

    /// A sealed offer of `udp`, issued now, and its id.
    fn offer(&self, udp: Vec<SocketAddr>) -> (SealedCandidates, OfferId) {
        self.offer_at(unix_now_ms(), udp)
    }

    fn offer_at(&self, issued_at_ms: u64, udp: Vec<SocketAddr>) -> (SealedCandidates, OfferId) {
        let offer = DeviceOffer::new(issued_at_ms, udp);
        let sealed = self.sealer().seal_offer(NODE_ID, &offer).unwrap();
        (sealed, offer.offer_id)
    }

    /// Hands `udp` to the runtime as a fresh offer and opens its answer.
    fn answered(
        &self,
        runtime: &mut CarrierRuntime,
        udp: Vec<SocketAddr>,
        register: Option<UdpRendezvous>,
    ) -> (GatewayAnswer, PunchId, OfferId) {
        let (sealed, offer_id) = self.offer(udp);
        let punch_id = PunchId::generate();
        let ControlReport::DirectAnswer {
            punch_id: echoed,
            answer,
        } = runtime.handle_offer(punch_id, sealed, register)
        else {
            panic!("the offer was declined");
        };
        assert_eq!(echoed, punch_id);
        let answer = self
            .sealer()
            .open_answer(NODE_ID, &punch_id, &offer_id, &answer)
            .unwrap();
        (answer, punch_id, offer_id)
    }

    fn keys(&self) -> PhoneKeys<'_> {
        PhoneKeys {
            device: &self.device,
            gateway_public: &self.gateway_public,
        }
    }

    async fn session(
        &self,
        connection: &quinn::Connection,
        token: &DirectToken,
        class: LegClass,
    ) -> QuicSession {
        self.keys().session(connection, token, class).await
    }

    async fn observed_handshake(&self) -> (Vec<u8>, Vec<u8>) {
        let gateway = load_or_create_static_keypair(&self.state.secret_vault)
            .await
            .unwrap();
        let mut responder = gateway.ik_responder().unwrap();
        let mut initiator = self.device.ik_initiator(&self.gateway_public).unwrap();
        let mut buffer = vec![0u8; NOISE_MAX_MESSAGE];
        let mut scratch = vec![0u8; NOISE_MAX_MESSAGE];
        let len = initiator.write_message(&[], &mut buffer).unwrap();
        let msg1 = buffer[..len].to_vec();
        responder.read_message(&msg1, &mut scratch).unwrap();
        let len = responder.write_message(&[], &mut buffer).unwrap();
        initiator
            .read_message(&buffer[..len], &mut scratch)
            .unwrap();
        let mut transport = initiator.into_transport_mode().unwrap();
        let len = transport.write_message(&[], &mut buffer).unwrap();
        (msg1, buffer[..len].to_vec())
    }
}

/// A blob store that records how each upload's `put_stream` future ended: by
/// reaching the end of its body, or by being dropped before it.
#[derive(Default)]
struct WatchedUploads {
    blobs: MemoryBlobStore,
    entered: AtomicBool,
    body_ended: AtomicBool,
    ended: AtomicBool,
}

struct SetOnDrop<'a>(&'a AtomicBool);

impl Drop for SetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl BlobStore for WatchedUploads {
    async fn put(
        &self,
        bytes: &[u8],
        mime_type: &str,
        uploader_identity: Option<&str>,
    ) -> BlobResult<BlobRef> {
        self.blobs.put(bytes, mime_type, uploader_identity).await
    }

    async fn put_stream(
        &self,
        mut stream: ByteStream,
        mime_type: &str,
        uploader_identity: Option<&str>,
        _max_bytes: u64,
    ) -> BlobResult<BlobRef> {
        let _ended = SetOnDrop(&self.ended);
        self.entered.store(true, Ordering::SeqCst);
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            body.extend_from_slice(
                &chunk.map_err(|error| StorageError::Storage(error.to_string()))?,
            );
        }
        self.body_ended.store(true, Ordering::SeqCst);
        self.blobs.put(&body, mime_type, uploader_identity).await
    }

    async fn get(&self, blob_id: &str) -> BlobResult<Vec<u8>> {
        self.blobs.get(blob_id).await
    }

    async fn open(&self, blob_id: &str) -> BlobResult<BlobReader> {
        self.blobs.open(blob_id).await
    }

    async fn stat(&self, blob_id: &str) -> BlobResult<BlobMeta> {
        self.blobs.stat(blob_id).await
    }

    async fn delete(&self, blob_id: &str) -> BlobResult<()> {
        self.blobs.delete(blob_id).await
    }

    async fn list_ids_by_uploader(
        &self,
        prefix: &str,
        older_than_us: Option<i64>,
    ) -> BlobResult<Vec<String>> {
        self.blobs.list_ids_by_uploader(prefix, older_than_us).await
    }
}

/// An upload whose body never comes, so its handler stays parked reading it
/// for longer than a test runs, unless a revoke drops it.
fn parked_upload() -> TunnelRequest {
    TunnelRequest::Head {
        request_id: 1,
        method: "POST".to_owned(),
        path: "/v1/blobs".to_owned(),
        headers: Vec::new(),
        body_len: Some(BLOB_BODY_LEN),
    }
}

async fn udp_peer() -> (UdpSocket, SocketAddrV4) {
    let socket = UdpSocket::bind(LOOPBACK).await.unwrap();
    let SocketAddr::V4(address) = socket.local_addr().unwrap() else {
        panic!("an IPv4 bind has an IPv4 address");
    };
    (socket, address)
}

/// The next probe datagram `socket` receives, and its sender.
async fn next_probe(socket: &UdpSocket) -> (ProbeDatagram, SocketAddr) {
    let mut buffer = [0u8; UDP_BUFFER_LEN];
    let (len, source) = timeout(STEP, socket.recv_from(&mut buffer))
        .await
        .expect("a probe datagram arrives")
        .unwrap();
    (ProbeDatagram::decode(&buffer[..len]).unwrap(), source)
}

/// Whether `socket` receives nothing for [`QUIET`].
async fn stays_silent(socket: &UdpSocket) -> bool {
    let mut buffer = [0u8; UDP_BUFFER_LEN];
    timeout(QUIET, socket.recv_from(&mut buffer)).await.is_err()
}

async fn count_punches(socket: &UdpSocket, expected: usize) {
    for _ in 0..expected {
        let (datagram, _) = next_probe(socket).await;
        assert!(matches!(datagram, ProbeDatagram::Punch { .. }));
    }
}

#[tokio::test]
async fn a_runtime_holds_its_udp_socket_and_advertises_it_until_stopped() {
    let pairing = Pairing::new().await;
    let mut process = CarrierProcess::new();
    let runtime = pairing
        .runtime_with(&loopback(), &mut process, CarrierTiming::PRODUCTION)
        .await;
    assert_eq!(
        runtime.capability(),
        Some(DirectCapability {
            version: DIRECT_PROTOCOL_VERSION,
            udp: true,
        })
    );
    assert!(
        process.identity().is_some(),
        "a UDP socket needs the QUIC certificate"
    );
    let bound = udp_address(&runtime);
    assert_ne!(bound.port(), 0);
    assert!(port_is_held(bound));

    runtime.stop().await;
    assert!(
        !port_is_held(bound),
        "stop returns once the socket is closed"
    );
}

#[tokio::test]
async fn a_stopped_runtime_leaves_its_fixed_port_to_the_next() {
    let pairing = Pairing::new().await;
    let mut process = CarrierProcess::new();
    let first = pairing
        .runtime_with(&loopback(), &mut process, CarrierTiming::PRODUCTION)
        .await;
    let bound = udp_address(&first);
    first.stop().await;

    let fixed = udp_only(Some(&bound.to_string()), None);
    let next = pairing
        .runtime_with(&fixed, &mut process, CarrierTiming::PRODUCTION)
        .await;
    assert_eq!(active(&next).bound_addresses(), vec![bound]);
    next.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_draining_at_stop_still_leaves_the_port_free() {
    let pairing = Pairing::new().await;
    let mut process = CarrierProcess::new();
    let mut runtime = pairing
        .runtime_with(&loopback(), &mut process, CarrierTiming::PRODUCTION)
        .await;
    let bound = udp_address(&runtime);
    let phone = Phone::bind(socket(LOOPBACK));
    let (answer, _, _) = pairing.answered(&mut runtime, vec![phone.address()], None);
    let connection = phone.connect(answer.quic_cert_sha256, bound).await;
    let _session = pairing
        .session(&connection, &answer.token, LegClass::Api)
        .await;

    runtime.stop().await;
    assert!(!port_is_held(bound), "the drain ended before stop returned");
    assert_eq!(close_code(&connection).await, CARRIER_REVOKED);
    let next = pairing
        .runtime_with(
            &udp_only(Some(&bound.to_string()), None),
            &mut process,
            CarrierTiming::PRODUCTION,
        )
        .await;
    assert_eq!(active(&next).bound_addresses(), vec![bound]);
    next.stop().await;
}

#[tokio::test]
async fn every_runtime_mints_its_own_token_and_shares_the_processs_offer_gate() {
    let pairing = Pairing::new().await;
    let before = unix_now_ms();
    let mut process = CarrierProcess::new();
    let after = unix_now_ms();
    let first = pairing
        .runtime_with(&loopback(), &mut process, CarrierTiming::PRODUCTION)
        .await;
    let second = pairing
        .runtime_with(&loopback(), &mut process, CarrierTiming::PRODUCTION)
        .await;
    assert!(context(&first).token != context(&second).token);
    assert!((before..=after).contains(&process.offers().started_at_ms()));
    for runtime in [&first, &second] {
        assert_eq!(
            active(runtime).gate.started_at_ms(),
            process.offers().started_at_ms()
        );
    }
    first.stop().await;
    second.stop().await;
}

/// A Reconfigure starts a new runtime in the same process, and a replay C
/// held from before it is still declined, even one that P's fast clock
/// issued after the new runtime started: the replay cache is the process's.
#[tokio::test]
async fn an_offer_one_runtime_accepted_is_a_replay_to_the_next() {
    let pairing = Pairing::new().await;
    let mut process = CarrierProcess::new();
    let mut first = pairing
        .runtime_with(&loopback(), &mut process, CarrierTiming::PRODUCTION)
        .await;
    let lead = u64::try_from(OFFER_MAX_AGE.as_millis()).unwrap() / 2;
    let (sealed, _) = pairing.offer_at(unix_now_ms() + lead, Vec::new());
    let punch_id = PunchId::generate();
    assert!(matches!(
        first.handle_offer(punch_id, sealed.clone(), None),
        ControlReport::DirectAnswer { .. }
    ));
    first.stop().await;

    let mut next = pairing
        .runtime_with(&loopback(), &mut process, CarrierTiming::PRODUCTION)
        .await;
    let punch_id = PunchId::generate();
    assert_eq!(
        next.handle_offer(punch_id, sealed, None),
        ControlReport::DirectDeclined { punch_id },
        "declined:replayed"
    );
    next.stop().await;
}

#[tokio::test]
async fn one_certificate_serves_every_runtime_of_the_process() {
    let pairing = Pairing::new().await;
    let mut process = CarrierProcess::new();
    pairing
        .runtime_with(&loopback(), &mut process, CarrierTiming::PRODUCTION)
        .await
        .stop()
        .await;
    let first = process.identity().map(ServerIdentity::cert_hash);
    pairing
        .runtime_with(&loopback(), &mut process, CarrierTiming::PRODUCTION)
        .await
        .stop()
        .await;
    assert!(first.is_some());
    assert!(first == process.identity().map(ServerIdentity::cert_hash));
}

#[tokio::test]
async fn a_runtime_with_no_carrier_configured_binds_nothing_and_advertises_nothing() {
    let pairing = Pairing::new().await;
    let nothing = RuntimeCarrierConfig { udp: None };
    for config in [nothing, udp_only(None, None)] {
        let mut process = CarrierProcess::new();
        let runtime = CarrierRuntime::start(
            &config,
            &mut process,
            &pairing.state,
            NODE_ID,
            BindingDevice {
                device_id: &pairing.device_id,
                device_pubkey: &pairing.device.public(),
            },
        )
        .await
        .unwrap();
        assert_eq!(runtime.capability(), None);
        assert!(runtime.active.is_none());
        assert!(
            process.identity().is_none(),
            "no certificate with nothing to bind"
        );
    }
}

#[tokio::test]
async fn a_binding_without_candidate_keys_stays_inactive() {
    let pairing = Pairing::new().await;
    let runtime = CarrierRuntime::start(
        &loopback(),
        &mut CarrierProcess::new(),
        &pairing.state,
        NODE_ID,
        BindingDevice {
            device_id: &pairing.device_id,
            device_pubkey: &[7; KEY_LEN - 1],
        },
    )
    .await
    .expect("a malformed device key is no reason to retry");
    assert_eq!(runtime.capability(), None);
    assert!(runtime.active.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_udp_family_that_fails_to_bind_is_bound_again_after_the_rebind_delay() {
    let pairing = Pairing::new().await;
    let occupied = StdUdpSocket::bind(LOOPBACK).unwrap();
    let address = occupied.local_addr().unwrap();
    let timing = CarrierTiming {
        rebind_delay: TEST_REBIND_DELAY,
        ..CarrierTiming::PRODUCTION
    };
    let mut runtime = pairing
        .runtime_with(
            &udp_only(Some(&address.to_string()), None),
            &mut CarrierProcess::new(),
            timing,
        )
        .await;
    assert_eq!(
        runtime.capability(),
        Some(DirectCapability {
            version: DIRECT_PROTOCOL_VERSION,
            udp: true,
        }),
        "the first hello names the family it keeps binding, so C routes its offers once it binds"
    );
    let (sealed, _) = pairing.offer(Vec::new());
    let punch_id = PunchId::generate();
    assert_eq!(
        runtime.handle_offer(punch_id, sealed, None),
        ControlReport::DirectDeclined { punch_id },
        "declined:unbound"
    );

    tokio::time::sleep(TEST_REBIND_DELAY * 3).await;
    assert!(
        active(&runtime).bound_addresses().is_empty(),
        "every retry fails while the port is held, and the family keeps retrying"
    );
    drop(occupied);
    until("the family is bound again", || {
        active(&runtime).bound_addresses() == vec![address]
    })
    .await;
    let phone = Phone::bind(socket(LOOPBACK));
    let (answer, _, _) = pairing.answered(&mut runtime, vec![phone.address()], None);
    let connection = phone.connect(answer.quic_cert_sha256, address).await;
    pairing
        .session(&connection, &answer.token, LegClass::Api)
        .await;
    runtime.stop().await;
}

#[tokio::test]
async fn an_ipv6_only_runtime_offers_no_udp_registration() {
    if !ipv6_loopback_available() {
        eprintln!("skipping: no IPv6 loopback on this host");
        return;
    }
    let pairing = Pairing::new().await;
    let runtime = pairing.runtime(&udp_only(None, Some("[::1]:0"))).await;
    assert_eq!(
        runtime.capability(),
        Some(DirectCapability {
            version: DIRECT_PROTOCOL_VERSION,
            udp: false,
        })
    );
    runtime.stop().await;
}

#[tokio::test]
async fn an_inactive_runtime_declines_every_offer() {
    let pairing = Pairing::new().await;
    let mut runtime = CarrierRuntime::inactive();
    let (sealed, _) = pairing.offer(Vec::new());
    let punch_id = PunchId::generate();
    assert_eq!(
        runtime.handle_offer(punch_id, sealed, None),
        ControlReport::DirectDeclined { punch_id }
    );
}

#[tokio::test]
async fn an_accepted_offer_is_answered_with_the_token_certificate_and_candidates() {
    let pairing = Pairing::new().await;
    let mut process = CarrierProcess::new();
    let mut runtime = pairing
        .runtime_with(&loopback(), &mut process, CarrierTiming::PRODUCTION)
        .await;
    let (answer, _, offer_id) = pairing.answered(&mut runtime, Vec::new(), None);
    assert_eq!(answer.offer_id, offer_id);
    assert!(answer.token == context(&runtime).token);
    assert!(Some(answer.quic_cert_sha256) == process.identity().map(ServerIdentity::cert_hash));
    assert!(
        answer.udp.contains(&udp_address(&runtime)),
        "the loopback socket's own address: {:?}",
        answer.udp
    );
    runtime.stop().await;
}

#[tokio::test]
async fn offers_that_fail_the_seal_freshness_or_replay_rules_are_declined() {
    let pairing = Pairing::new().await;
    let mut runtime = pairing.runtime(&loopback()).await;
    let started_at_ms = active(&runtime).gate.started_at_ms();
    let declined = |runtime: &mut CarrierRuntime, sealed: SealedCandidates| {
        let punch_id = PunchId::generate();
        runtime.handle_offer(punch_id, sealed, None) == ControlReport::DirectDeclined { punch_id }
    };

    let garbage = SealedCandidates {
        n: "bm9uY2U=".to_owned(),
        enc: "Y2lwaGVydGV4dA==".to_owned(),
    };
    assert!(declined(&mut runtime, garbage), "declined:auth");
    let stranger = StaticKeypair::generate().unwrap();
    let foreign = DeviceSealer::derive(&stranger.secret(), &pairing.gateway_public)
        .unwrap()
        .seal_offer(NODE_ID, &DeviceOffer::new(unix_now_ms(), Vec::new()))
        .unwrap();
    assert!(declined(&mut runtime, foreign), "another device's key");
    let other_node = pairing
        .sealer()
        .seal_offer("node-2", &DeviceOffer::new(unix_now_ms(), Vec::new()))
        .unwrap();
    assert!(declined(&mut runtime, other_node), "another node's offer");

    let max_age = u64::try_from(OFFER_MAX_AGE.as_millis()).unwrap();
    let (old, _) = pairing.offer_at(unix_now_ms() - 2 * max_age, Vec::new());
    assert!(declined(&mut runtime, old), "declined:stale");
    let (before_start, _) = pairing.offer_at(started_at_ms - 1, Vec::new());
    assert!(
        declined(&mut runtime, before_start),
        "an offer issued before the runtime started"
    );

    let (fresh, _) = pairing.offer(Vec::new());
    assert!(!declined(&mut runtime, fresh.clone()));
    assert!(declined(&mut runtime, fresh), "declined:replayed");
    runtime.stop().await;
}

#[tokio::test]
async fn a_full_replay_cache_declines_new_offers() {
    let pairing = Pairing::new().await;
    let mut runtime = pairing.runtime(&loopback()).await;
    for _ in 0..MAX_REPLAY_ENTRIES {
        pairing.answered(&mut runtime, Vec::new(), None);
    }
    let (sealed, _) = pairing.offer(Vec::new());
    let punch_id = PunchId::generate();
    assert_eq!(
        runtime.handle_offer(punch_id, sealed, None),
        ControlReport::DirectDeclined { punch_id },
        "declined:over_cap"
    );
    runtime.stop().await;
}

#[tokio::test]
async fn a_new_offer_at_the_cap_supersedes_the_oldest_punch() {
    let pairing = Pairing::new().await;
    let mut runtime = pairing.runtime(&loopback()).await;
    let hosts = ["198.18.0.1", "198.18.0.2", "198.18.0.3"];
    for (index, ip) in hosts.iter().enumerate() {
        let port = 40_000 + u16::try_from(index).unwrap();
        pairing.answered(
            &mut runtime,
            vec![SocketAddr::new(ip.parse().unwrap(), port)],
            None,
        );
    }
    assert!(
        !admits(&runtime, hosts[0]),
        "the oldest punch is superseded"
    );
    assert!(admits(&runtime, hosts[1]));
    assert!(admits(&runtime, hosts[2]));
    runtime.stop().await;
}

#[tokio::test]
async fn an_excluded_device_candidate_is_dropped_and_never_admitted() {
    let pairing = Pairing::new().await;
    let mut runtime = pairing.runtime(&loopback()).await;
    pairing.answered(
        &mut runtime,
        vec![
            socket("[fe80::1]:40000"),
            socket("203.0.113.9:40000"),
            socket("198.18.0.7:0"),
            host(40_001),
        ],
        None,
    );
    assert!(!admits(&runtime, "fe80::1"));
    assert!(!admits(&runtime, "203.0.113.9"), "a documentation address");
    assert!(!admits(&runtime, "198.18.0.7"), "no port");
    assert!(admits(&runtime, UNREACHED_HOST));
    runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accepted_offer_punches_the_device_candidates_and_a_declined_one_nothing() {
    let pairing = Pairing::new().await;
    let mut runtime = pairing.runtime(&loopback()).await;
    let (device, device_address) = udp_peer().await;

    let garbage = SealedCandidates {
        n: "bm9uY2U=".to_owned(),
        enc: "Y2lwaGVydGV4dA==".to_owned(),
    };
    runtime.handle_offer(PunchId::generate(), garbage, None);
    let (stale, _) = pairing.offer_at(0, vec![SocketAddr::V4(device_address)]);
    runtime.handle_offer(PunchId::generate(), stale, None);
    assert!(
        stays_silent(&device).await,
        "a declined offer is never punched"
    );

    pairing.answered(&mut runtime, vec![SocketAddr::V4(device_address)], None);
    count_punches(&device, PUNCH_BURST).await;
    assert!(stays_silent(&device).await, "one burst per pair");
    runtime.stop().await;
}

/// C's rendezvous for one punch, answering A's `Register`s by script.
struct MockRendezvous {
    socket: UdpSocket,
    address: SocketAddrV4,
    ticket: RendezvousTicket,
}

impl MockRendezvous {
    async fn start() -> Self {
        let (socket, address) = udp_peer().await;
        Self {
            socket,
            address,
            ticket: RendezvousTicket::from_bytes([9; RENDEZVOUS_TICKET_LEN]),
        }
    }

    fn register(&self) -> UdpRendezvous {
        UdpRendezvous {
            address: self.address.to_string(),
            ticket: self.ticket.clone(),
        }
    }

    /// The next `Register` from A, checked, and where it came from.
    async fn next_register(&self, punch_id: PunchId) -> SocketAddr {
        let (datagram, source) = next_probe(&self.socket).await;
        assert_eq!(
            datagram,
            ProbeDatagram::Register {
                punch_id,
                role: PunchRole::Gateway,
                ticket: self.ticket.clone(),
            }
        );
        source
    }

    async fn reply(&self, datagram: ProbeDatagram, to: SocketAddr) {
        self.socket.send_to(&datagram.encode(), to).await.unwrap();
    }

    /// Discards what has already arrived.
    fn drain(&self) {
        let mut buffer = [0u8; UDP_BUFFER_LEN];
        while self.socket.try_recv_from(&mut buffer).is_ok() {}
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_peer_is_recovered_by_the_next_register_and_the_first_is_latched() {
    let pairing = Pairing::new().await;
    let mut runtime = pairing.runtime(&loopback()).await;
    let c = MockRendezvous::start().await;
    let (srflx, srflx_address) = udp_peer().await;
    let (conflict, conflict_address) = udp_peer().await;
    let (impostor, _) = udp_peer().await;

    let (_, punch_id, _) = pairing.answered(&mut runtime, vec![host(40_000)], Some(c.register()));
    let gateway = c.next_register(punch_id).await;
    assert_eq!(gateway, udp_address(&runtime), "from A's IPv4 socket");
    c.reply(ProbeDatagram::Registered { punch_id }, gateway)
        .await;
    // The second Register's Peer is lost; the third one's arrives.
    c.next_register(punch_id).await;
    c.next_register(punch_id).await;
    impostor
        .send_to(
            &ProbeDatagram::Peer {
                punch_id,
                srflx: conflict_address,
            }
            .encode(),
            gateway,
        )
        .await
        .unwrap();
    c.reply(
        ProbeDatagram::Peer {
            punch_id,
            srflx: srflx_address,
        },
        gateway,
    )
    .await;

    count_punches(&srflx, PUNCH_BURST).await;
    assert!(admits(&runtime, &srflx_address.ip().to_string()));
    c.reply(
        ProbeDatagram::Peer {
            punch_id,
            srflx: conflict_address,
        },
        gateway,
    )
    .await;
    assert!(
        stays_silent(&conflict).await,
        "neither a Peer from another source nor a conflicting one is punched"
    );
    c.drain();
    assert!(
        stays_silent(&c.socket).await,
        "a latched Peer ends the registration"
    );
    runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declined_offer_never_registers() {
    let pairing = Pairing::new().await;
    let mut runtime = pairing.runtime(&loopback()).await;
    let c = MockRendezvous::start().await;
    let (stale, _) = pairing.offer_at(0, Vec::new());
    let punch_id = PunchId::generate();
    assert_eq!(
        runtime.handle_offer(punch_id, stale, Some(c.register())),
        ControlReport::DirectDeclined { punch_id }
    );
    assert!(stays_silent(&c.socket).await);
    runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_private_peer_is_ignored_and_registration_goes_on() {
    let pairing = Pairing::new().await;
    let mut runtime = pairing.runtime(&loopback()).await;
    let c = MockRendezvous::start().await;
    let (_, punch_id, _) = pairing.answered(&mut runtime, vec![host(40_000)], Some(c.register()));
    let gateway = c.next_register(punch_id).await;
    let private = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 9), 40_000);
    c.reply(
        ProbeDatagram::Peer {
            punch_id,
            srflx: private,
        },
        gateway,
    )
    .await;
    c.next_register(punch_id).await;
    assert!(!admits(&runtime, "10.0.0.9"), "a private Peer is ignored");
    runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_incoming_outside_every_allowed_set_is_ignored() {
    let pairing = Pairing::new().await;
    let mut process = CarrierProcess::new();
    let mut runtime = pairing
        .runtime_with(&loopback(), &mut process, CarrierTiming::PRODUCTION)
        .await;
    let pinned = process.identity().unwrap().cert_hash();
    let target = udp_address(&runtime);
    let phone = Phone::bind(socket(LOOPBACK));
    assert!(phone.is_ignored(pinned, target).await, "no punch at all");

    pairing.answered(&mut runtime, vec![host(40_000)], None);
    assert!(
        phone.is_ignored(pinned, target).await,
        "another host's punch"
    );
    let counts = &context(&runtime).incoming;
    assert!(counts.ignored_not_in_punch.load(Ordering::Relaxed) > 0);
    assert_eq!(counts.retried.load(Ordering::Relaxed), 0, "no Retry either");
    runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_allowed_but_unvalidated_address_gets_a_retry_first() {
    let pairing = Pairing::new().await;
    let mut runtime = pairing.runtime(&loopback()).await;
    let phone = Phone::bind(socket(LOOPBACK));
    let (answer, _, _) = pairing.answered(&mut runtime, vec![phone.address()], None);
    let connection = phone
        .connect(answer.quic_cert_sha256, udp_address(&runtime))
        .await;
    let counts = &context(&runtime).incoming;
    assert!(counts.retried.load(Ordering::Relaxed) >= 1);
    assert_eq!(counts.accepted.load(Ordering::Relaxed), 1);
    connection.close(0u32.into(), b"");
    runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_allowed_source_at_its_cap_is_ignored_without_a_retry() {
    let pairing = Pairing::new().await;
    let mut runtime = pairing.runtime(&loopback()).await;
    let phone = Phone::bind(socket(LOOPBACK));
    let (answer, _, _) = pairing.answered(&mut runtime, vec![phone.address()], None);
    let target = udp_address(&runtime);
    let held: Vec<_> = (0..MAX_QUIC_CONNECTIONS_PER_SOURCE)
        .map(|_| {
            context(&runtime)
                .connections
                .try_acquire(phone.address().ip())
                .expect("a free slot")
        })
        .collect();

    assert!(phone.is_ignored(answer.quic_cert_sha256, target).await);
    let counts = &context(&runtime).incoming;
    assert_eq!(counts.retried.load(Ordering::Relaxed), 0, "no Retry either");
    assert!(counts.ignored_cap.load(Ordering::Relaxed) > 0);

    drop(held);
    let connection = phone.connect(answer.quic_cert_sha256, target).await;
    assert!(counts.retried.load(Ordering::Relaxed) >= 1);
    connection.close(0u32.into(), b"");
    runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_valid_authenticated_punch_admits_its_source_and_a_bad_tag_does_not() {
    let pairing = Pairing::new().await;
    let mut runtime = pairing.runtime(&loopback()).await;
    let phone = Phone::bind(socket(LOOPBACK));
    let target = udp_address(&runtime);
    let (answer, _, offer_id) = pairing.answered(&mut runtime, vec![host(40_000)], None);
    let phone_ip = phone.address().ip().to_string();

    let forged = ProbeDatagram::Punch {
        seq: 0,
        tag: PunchTag::from_bytes([0x5a; PUNCH_TAG_LEN]),
    };
    phone
        .socket
        .send_probe(&forged, target, None)
        .await
        .unwrap();
    assert!(phone.is_ignored(answer.quic_cert_sha256, target).await);
    assert!(!admits(&runtime, &phone_ip));

    let valid = ProbeDatagram::Punch {
        seq: 0,
        tag: pairing.sealer().punch_tag(&offer_id, 0).unwrap(),
    };
    phone.socket.send_probe(&valid, target, None).await.unwrap();
    until("the punch admits its source", || {
        admits(&runtime, &phone_ip)
    })
    .await;
    let connection = phone.connect(answer.quic_cert_sha256, target).await;
    pairing
        .session(&connection, &answer.token, LegClass::Api)
        .await;
    runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_preface_on_one_stream_does_not_block_the_next() {
    let pairing = Pairing::new().await;
    let timing = CarrierTiming {
        direct_open_deadline: STALLED_PREFACE_DEADLINE,
        first_stream_deadline: STALLED_PREFACE_DEADLINE,
        ..CarrierTiming::PRODUCTION
    };
    let mut runtime = pairing
        .runtime_with(&loopback(), &mut CarrierProcess::new(), timing)
        .await;
    let phone = Phone::bind(socket(LOOPBACK));
    let (answer, _, _) = pairing.answered(&mut runtime, vec![phone.address()], None);
    let connection = phone
        .connect(answer.quic_cert_sha256, udp_address(&runtime))
        .await;

    let (mut stalled, _stalled_recv) = connection.open_bi().await.unwrap();
    stalled.write_all(&[0, 0]).await.unwrap();
    timeout(
        STEP,
        pairing.session(&connection, &answer.token, LegClass::Api),
    )
    .await
    .expect("the second stream authenticates while the first stalls");
    runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_without_an_authenticated_stream_closes_at_the_first_stream_deadline() {
    let pairing = Pairing::new().await;
    let timing = CarrierTiming {
        first_stream_deadline: HANDSHAKE_DEADLINE,
        ..CarrierTiming::PRODUCTION
    };
    let mut runtime = pairing
        .runtime_with(&loopback(), &mut CarrierProcess::new(), timing)
        .await;
    let phone = Phone::bind(socket(LOOPBACK));
    let (answer, _, _) = pairing.answered(&mut runtime, vec![phone.address()], None);
    let connection = phone
        .connect(answer.quic_cert_sha256, udp_address(&runtime))
        .await;
    assert_eq!(close_code(&connection).await, CARRIER_UNAUTHENTICATED);
    until("the connection releases its slot", || {
        context(&runtime).connections.live() == 0
    })
    .await;
    runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_stream_that_fails_authentication_closes_the_connection() {
    let pairing = Pairing::new().await;
    let mut runtime = pairing.runtime(&loopback()).await;
    let phone = Phone::bind(socket(LOOPBACK));
    let (answer, _, _) = pairing.answered(&mut runtime, vec![phone.address()], None);
    let connection = phone
        .connect(answer.quic_cert_sha256, udp_address(&runtime))
        .await;
    let (mut send, _recv) = connection.open_bi().await.unwrap();
    write_direct_open(
        &mut send,
        &DirectOpen {
            token: DirectToken::generate(),
            class: LegClass::Api,
        },
    )
    .await
    .unwrap();
    assert_eq!(close_code(&connection).await, CARRIER_UNAUTHENTICATED);
    runtime.stop().await;
}

/// A QUIC long header's form bit, and its packet-type bits.
const LONG_HEADER: u8 = 0x80;
const LONG_PACKET_TYPE: u8 = 0x30;
const INITIAL_PACKET_TYPE: u8 = 0x00;

/// Relays a QUIC client to `server` until the server answers with an Initial,
/// which it does only once it has accepted the connection, and drops every
/// later client datagram, so the server's handshake never completes.
async fn handshake_blackhole(server: SocketAddr) -> SocketAddr {
    let proxy = UdpSocket::bind(LOOPBACK).await.unwrap();
    let address = proxy.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buffer = [0u8; UDP_BUFFER_LEN];
        let mut client = None;
        let mut server_accepted = false;
        while let Ok((len, source)) = proxy.recv_from(&mut buffer).await {
            if source == server {
                let first = buffer[0];
                server_accepted |=
                    first & LONG_HEADER != 0 && first & LONG_PACKET_TYPE == INITIAL_PACKET_TYPE;
                if let Some(client) = client {
                    let _ = proxy.send_to(&buffer[..len], client).await;
                }
            } else if !server_accepted {
                client = Some(source);
                let _ = proxy.send_to(&buffer[..len], server).await;
            }
        }
    });
    address
}

/// Without the deadline, the stalled handshake would hold its slot until
/// quinn's idle timeout, far past [`STEP`].
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handshake_that_never_completes_releases_its_slot_at_the_first_stream_deadline() {
    let pairing = Pairing::new().await;
    let timing = CarrierTiming {
        first_stream_deadline: SHORT_DEADLINE,
        ..CarrierTiming::PRODUCTION
    };
    let mut runtime = pairing
        .runtime_with(&loopback(), &mut CarrierProcess::new(), timing)
        .await;
    let phone = Phone::bind(socket(LOOPBACK));
    let (answer, _, _) = pairing.answered(&mut runtime, vec![phone.address()], None);
    let proxy = handshake_blackhole(udp_address(&runtime)).await;
    let _connecting = phone.connecting(answer.quic_cert_sha256, proxy);

    until("the gateway accepts the retried Initial", || {
        context(&runtime).incoming.accepted.load(Ordering::Relaxed) >= 1
    })
    .await;
    until("the stalled handshake releases its slot", || {
        context(&runtime).connections.live() == 0
    })
    .await;
    runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_a_binding_closes_its_active_quic_sessions_even_mid_request() {
    let uploads = Arc::new(WatchedUploads::default());
    let pairing = Pairing::new()
        .await
        .storing_blobs_in(Arc::clone(&uploads) as Arc<dyn BlobStore>);
    let mut runtime = pairing.runtime(&loopback()).await;
    let phone = Phone::bind(socket(LOOPBACK));
    let (answer, _, _) = pairing.answered(&mut runtime, vec![phone.address()], None);
    let connection = phone
        .connect(answer.quic_cert_sha256, udp_address(&runtime))
        .await;
    let mut chat = pairing
        .session(&connection, &answer.token, LegClass::Chat)
        .await;
    let mut upload = pairing
        .session(&connection, &answer.token, LegClass::Api)
        .await;
    upload.send_tunnel(&parked_upload()).await;
    until("the upload handler reads the body", || {
        uploads.entered.load(Ordering::SeqCst)
    })
    .await;

    let stopping = Instant::now();
    timeout(STEP, runtime.stop())
        .await
        .expect("stop does not wait for the parked request");
    assert!(stopping.elapsed() < STEP);
    assert!(
        uploads.ended.load(Ordering::SeqCst),
        "the handler is gone when stop returns"
    );
    assert!(
        !uploads.body_ended.load(Ordering::SeqCst),
        "the handler never reads the cut-off body as a complete one"
    );
    assert!(uploads.blobs.is_empty());
    assert_eq!(close_code(&connection).await, CARRIER_REVOKED);
    chat.ended().await;
    upload.ended().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_drops_every_task_before_closing_the_endpoints() {
    let pairing = Pairing::new().await;
    let mut process = CarrierProcess::new();
    let mut runtime = pairing
        .runtime_with(&loopback(), &mut process, CarrierTiming::PRODUCTION)
        .await;
    let bound = bound_socket(&runtime);
    let open_at_drop = Arc::new(Mutex::new(None));
    let guard = EndpointStateAtDrop {
        endpoint: bound.endpoint.clone(),
        open_at_drop: Arc::clone(&open_at_drop),
    };
    let runtime_active = runtime.active.as_mut().unwrap();
    let cancel = runtime_active.cancel.child_token();
    runtime_active.spawn_cancellable(cancel, async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    });
    drop(bound);

    let address = udp_address(&runtime);
    runtime.stop().await;
    assert_eq!(
        *open_at_drop.lock(),
        Some(true),
        "a task must be gone before its endpoint closes"
    );
    assert!(
        !port_is_held(address),
        "the endpoint is closed and released"
    );
}

/// Records, when its task is dropped, whether the endpoint was still open.
struct EndpointStateAtDrop {
    endpoint: quinn::Endpoint,
    open_at_drop: Arc<Mutex<Option<bool>>>,
}

impl Drop for EndpointStateAtDrop {
    fn drop(&mut self) {
        *self.open_at_drop.lock() = Some(endpoint_is_open(&self.endpoint));
    }
}

/// quinn refuses a new connection with `EndpointStopping` once the endpoint is
/// closed; before that, an IPv6 target on an IPv4 endpoint is refused as
/// invalid without any connection being created.
fn endpoint_is_open(endpoint: &quinn::Endpoint) -> bool {
    let client = client_config(
        ServerIdentity::generate().unwrap().cert_hash(),
        Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
    )
    .unwrap();
    match endpoint.connect_with(client, socket("[::1]:9"), DIRECT_QUIC_SERVER_NAME) {
        Err(quinn::ConnectError::EndpointStopping) => false,
        Err(quinn::ConnectError::InvalidRemoteAddress(_)) => true,
        other => panic!("unexpected probe outcome: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_persistent_receive_error_rebinds_the_family_and_it_keeps_serving() {
    let pairing = Pairing::new().await;
    let mut runtime = pairing.runtime(&loopback()).await;
    let phone = Phone::bind(socket(LOOPBACK));
    let (answer, _, _) = pairing.answered(&mut runtime, vec![phone.address()], None);
    let old = udp_address(&runtime);
    let connection = phone.connect(answer.quic_cert_sha256, old).await;
    let _session = pairing
        .session(&connection, &answer.token, LegClass::Api)
        .await;

    bound_socket(&runtime)
        .socket
        .inject_recv_errors(REBIND_AFTER_RECV_ERRORS);
    assert_eq!(close_code(&connection).await, CARRIER_REVOKED);
    until("the family is bound again", || {
        active(&runtime)
            .bound_addresses()
            .first()
            .is_some_and(|address| *address != old)
    })
    .await;
    assert!(runtime.capability().is_some());
    let (answer, _, _) = pairing.answered(&mut runtime, vec![phone.address()], None);
    let fresh = phone
        .connect(answer.quic_cert_sha256, udp_address(&runtime))
        .await;
    pairing.session(&fresh, &answer.token, LegClass::Api).await;
    runtime.stop().await;
}

/// The in-memory halves of a relay data leg.
struct ChannelSink(mpsc::Sender<Vec<u8>>);
struct ChannelSource(mpsc::Receiver<Vec<u8>>);

#[async_trait::async_trait]
impl BinarySink for ChannelSink {
    async fn send_bytes(&mut self, bytes: Vec<u8>) -> Result<(), ()> {
        self.0.send(bytes).await.map_err(|_| ())
    }

    async fn close(&mut self) {}
}

#[async_trait::async_trait]
impl BinarySource for ChannelSource {
    async fn next_bytes(&mut self) -> Option<Vec<u8>> {
        self.0.recv().await
    }
}

/// A content session over an in-memory relay leg, deduped by its own abort
/// handle as `relay_content` runs one; its halves are the phone's.
fn spawn_relay_chat_session(
    pairing: &Pairing,
) -> (
    JoinHandle<()>,
    mpsc::Sender<Vec<u8>>,
    mpsc::Receiver<Vec<u8>>,
) {
    let (to_gateway, from_phone) = mpsc::channel(8);
    let (to_phone, from_gateway) = mpsc::channel(8);
    let (abort_tx, abort_rx) = oneshot::channel();
    let state = pairing.state.clone();
    let leg = tokio::spawn(async move {
        let dedup = abort_rx.await.ok().map(|abort| LegDedup {
            registry: state.device_leg_registry.clone(),
            abort,
        });
        let _ = run_content_session(
            ChannelSink(to_phone),
            ChannelSource(from_phone),
            &state,
            dedup,
        )
        .await;
    });
    abort_tx.send(leg.abort_handle()).unwrap();
    (leg, to_gateway, from_gateway)
}

/// A live relay chat leg: it completes Noise IK and sends its first
/// transport message, a `Pong`, which draws no reply; returns once the
/// gateway has installed it as the device's live chat leg.
async fn relay_chat_leg(pairing: &Pairing) -> (JoinHandle<()>, mpsc::Sender<Vec<u8>>) {
    let (leg, to_gateway, mut from_gateway) = spawn_relay_chat_session(pairing);
    let mut handshake = pairing
        .device
        .ik_initiator(&pairing.gateway_public)
        .unwrap();
    let mut buffer = vec![0u8; NOISE_MAX_MESSAGE];
    let len = handshake.write_message(&[], &mut buffer).unwrap();
    to_gateway.send(buffer[..len].to_vec()).await.unwrap();
    let reply = timeout(STEP, from_gateway.recv()).await.unwrap().unwrap();
    handshake.read_message(&reply, &mut buffer).unwrap();
    let mut transport = handshake.into_transport_mode().unwrap();
    let pong = wire::encode(&Frame::Pong).unwrap();
    for message in write_chunked(&mut transport, &pong).unwrap() {
        to_gateway.send(message).await.unwrap();
    }
    let leg_id = leg.id();
    until("the gateway installs the relay chat leg", || {
        pairing
            .state
            .device_leg_registry
            .get(&pairing.device_id)
            .is_some_and(|live| live.id() == leg_id)
    })
    .await;
    (leg, to_gateway)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leg_dedup_spans_the_relay_and_a_carrier() {
    let pairing = Pairing::new().await;
    let mut runtime = pairing.runtime(&loopback()).await;
    let phone = Phone::bind(socket(LOOPBACK));
    let (answer, _, _) = pairing.answered(&mut runtime, vec![phone.address()], None);
    let connection = phone
        .connect(answer.quic_cert_sha256, udp_address(&runtime))
        .await;

    let (relay, _relay_writer) = relay_chat_leg(&pairing).await;
    let mut carrier_chat = pairing
        .session(&connection, &answer.token, LegClass::Chat)
        .await;
    let displaced = timeout(STEP, relay).await.expect("the relay leg ends");
    assert!(
        displaced.unwrap_err().is_cancelled(),
        "the carrier chat leg displaces the relay one"
    );

    let (next_relay, _next_writer) = relay_chat_leg(&pairing).await;
    carrier_chat.ended().await;
    assert!(
        connection.close_reason().is_none(),
        "only the displaced stream ends, not the carrier"
    );
    next_relay.abort();
    runtime.stop().await;
}

/// C opens every relay data leg and sees its Noise msg1, which carries no
/// replay protection. A replayed one gets A's msg2, but the relay leg has no
/// handshake confirmation, so its dedup waits for a first transport message
/// that only the real initiator can produce: the live carrier chat leg,
/// which C cannot reach otherwise, is never displaced.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replayed_relay_handshake_never_displaces_the_live_carrier_chat_leg() {
    let pairing = Pairing::new().await;
    let mut runtime = pairing.runtime(&loopback()).await;
    let phone = Phone::bind(socket(LOOPBACK));
    let (answer, _, _) = pairing.answered(&mut runtime, vec![phone.address()], None);
    let connection = phone
        .connect(answer.quic_cert_sha256, udp_address(&runtime))
        .await;
    let mut carrier_chat = pairing
        .session(&connection, &answer.token, LegClass::Chat)
        .await;
    let (msg1, confirmation) = pairing.observed_handshake().await;

    let (replayed, to_gateway, mut from_gateway) = spawn_relay_chat_session(&pairing);
    let replayed_id = replayed.id();
    to_gateway.send(msg1).await.unwrap();
    timeout(STEP, from_gateway.recv())
        .await
        .expect("the gateway answers a replayed msg1")
        .expect("handshake message 2");
    tokio::time::sleep(QUIET).await;
    assert!(
        pairing
            .state
            .device_leg_registry
            .get(&pairing.device_id)
            .is_some_and(|live| live.id() != replayed_id),
        "the replay displaced the live carrier chat leg"
    );
    to_gateway.send(confirmation).await.unwrap();
    timeout(STEP, replayed)
        .await
        .expect("a message sealed for another session ends the replay")
        .unwrap();

    let ping = wire::encode(&Frame::Ping).unwrap();
    carrier_chat.send_plaintext(&ping).await;
    assert_eq!(
        wire::decode(&carrier_chat.recv_plaintext().await).unwrap(),
        Frame::Pong,
        "the carrier chat leg still serves"
    );
    runtime.stop().await;
}

type ListedLeg = (LegClass, Option<CarrierKind>);

/// The pairing device's legs in the link table, and its last offer's outcome.
fn listed(pairing: &Pairing) -> (Vec<ListedLeg>, Option<Result<(), Decline>>) {
    pairing
        .state
        .device_links
        .snapshot()
        .into_iter()
        .find(|record| record.device_id == pairing.device_id)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_link_table_lists_each_carrier_leg_by_kind_and_each_offers_outcome() {
    let pairing = Pairing::new().await;
    let mut runtime = pairing.runtime(&loopback()).await;
    let phone = Phone::bind(socket(LOOPBACK));
    let (sealed, offer_id) = pairing.offer(vec![phone.address()]);
    let punch_id = PunchId::generate();
    let ControlReport::DirectAnswer { answer, .. } =
        runtime.handle_offer(punch_id, sealed.clone(), None)
    else {
        panic!("the offer was declined");
    };
    let answer = pairing
        .sealer()
        .open_answer(NODE_ID, &punch_id, &offer_id, &answer)
        .unwrap();
    assert_eq!(listed(&pairing), (Vec::new(), Some(Ok(()))));

    let connection = phone
        .connect(answer.quic_cert_sha256, udp_address(&runtime))
        .await;
    let quic_chat = pairing
        .session(&connection, &answer.token, LegClass::Chat)
        .await;
    let quic_api = pairing
        .session(&connection, &answer.token, LegClass::Api)
        .await;
    until("both carrier legs are listed", || {
        listed(&pairing).0.len() == 2
    })
    .await;
    let (mut legs, _) = listed(&pairing);
    legs.sort_by_key(|(class, _)| class.as_str());
    assert_eq!(
        legs,
        [
            (LegClass::Api, Some(CarrierKind::Ipv4)),
            (LegClass::Chat, Some(CarrierKind::Ipv4)),
        ],
        "a loopback QUIC pair is public IPv4 on both ends under the test policy"
    );

    let replayed = PunchId::generate();
    assert_eq!(
        runtime.handle_offer(replayed, sealed, None),
        ControlReport::DirectDeclined { punch_id: replayed }
    );
    assert_eq!(listed(&pairing).1, Some(Err(Decline::Replayed)));

    drop((quic_chat, quic_api));
    until("ended legs leave the table", || {
        listed(&pairing).0.is_empty()
    })
    .await;
    assert_eq!(
        listed(&pairing).1,
        Some(Err(Decline::Replayed)),
        "the last offer outlives the legs"
    );
    runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quic_leg_on_a_wildcard_socket_takes_its_kind_from_the_address_the_phone_dialed() {
    let pairing = Pairing::new().await;
    let mut runtime = pairing.runtime(&udp_only(Some(WILDCARD_V4), None)).await;
    let bound = udp_address(&runtime);
    assert!(bound.ip().is_unspecified(), "{bound} is the wildcard");
    let phone = Phone::bind(socket(LOOPBACK));
    let (answer, _, _) = pairing.answered(&mut runtime, vec![phone.address()], None);

    let connection = phone
        .connect(
            answer.quic_cert_sha256,
            SocketAddr::from((Ipv4Addr::LOCALHOST, bound.port())),
        )
        .await;
    let _chat = pairing
        .session(&connection, &answer.token, LegClass::Chat)
        .await;
    until("the chat leg is listed", || !listed(&pairing).0.is_empty()).await;
    assert_eq!(
        listed(&pairing).0,
        [(LegClass::Chat, Some(CarrierKind::Ipv4))],
        "the socket reports each datagram's destination, 127.0.0.1"
    );
    runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replayed_carrier_handshake_never_displaces_the_live_chat_leg() {
    let pairing = Pairing::new().await;
    let mut runtime = pairing.runtime(&loopback()).await;
    let phone = Phone::bind(socket(LOOPBACK));
    let (answer, _, _) = pairing.answered(&mut runtime, vec![phone.address()], None);
    let connection = phone
        .connect(answer.quic_cert_sha256, udp_address(&runtime))
        .await;
    // An authenticated stream first, so the replay's failure does not close
    // the connection the genuine chat leg then opens on.
    let _api = pairing
        .session(&connection, &answer.token, LegClass::Api)
        .await;
    let (mut live, _live_writer) = relay_chat_leg(&pairing).await;
    let (msg1, confirmation) = pairing.observed_handshake().await;

    let (mut send, recv) = connection.open_bi().await.unwrap();
    let mut frames = FrameReader::new(recv);
    write_direct_open(
        &mut send,
        &DirectOpen {
            token: answer.token.clone(),
            class: LegClass::Chat,
        },
    )
    .await
    .unwrap();
    write_frame(&mut send, &msg1).await.unwrap();
    timeout(STEP, frames.next_frame())
        .await
        .expect("the gateway answers a replayed msg1")
        .unwrap()
        .expect("handshake message 2");
    write_frame(&mut send, &confirmation).await.unwrap();
    let after_confirmation = timeout(STEP, frames.next_frame())
        .await
        .expect("the gateway ends the replay");
    assert!(
        !matches!(after_confirmation, Ok(Some(_))),
        "the replay is closed without another frame"
    );
    assert!(
        timeout(QUIET, &mut live).await.is_err(),
        "the live chat leg is not displaced"
    );

    let _chat = pairing
        .session(&connection, &answer.token, LegClass::Chat)
        .await;
    let displaced = timeout(STEP, live).await.expect("the relay leg ends");
    assert!(
        displaced.unwrap_err().is_cancelled(),
        "a confirmed carrier chat leg does displace it"
    );
    runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quic_api_session_runs_the_tunnel() {
    let pairing = Pairing::new().await;
    let mut runtime = pairing.runtime(&loopback()).await;
    let phone = Phone::bind(socket(LOOPBACK));
    let (answer, _, _) = pairing.answered(&mut runtime, vec![phone.address()], None);
    let connection = phone
        .connect(answer.quic_cert_sha256, udp_address(&runtime))
        .await;
    let mut session = pairing
        .session(&connection, &answer.token, LegClass::Api)
        .await;
    session
        .send_tunnel(&TunnelRequest::Head {
            request_id: 1,
            method: "GET".to_owned(),
            path: "/v1/chat/sessions".to_owned(),
            headers: Vec::new(),
            body_len: None,
        })
        .await;
    match session.recv_tunnel().await {
        TunnelResponse::Head {
            request_id, status, ..
        } => assert_eq!((request_id, status), (1, 200)),
        other => panic!("expected a response head, got {other:?}"),
    }
    runtime.stop().await;
}
