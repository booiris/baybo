use std::net::{Ipv6Addr, SocketAddrV6};

use remote_host_protocol::relay::{
    ControlReport, DIRECT_PROTOCOL_VERSION, DirectCapability, PunchId, PunchRole, RendezvousKey,
    SealedCandidates,
};
use tokio::sync::mpsc;

use super::*;
use crate::control::{ControlSignal, OfferSource};
use crate::punch::{MAX_DATAGRAMS_PER_PUNCH, OfferOutcome};

const NODE: &str = "node-1";
const KEY: &str = "inst-A";

type Inbound = io::Result<(Vec<u8>, SocketAddr)>;

/// A scripted socket: datagrams and receive errors in, replies out.
struct FakeIo {
    inbound: tokio::sync::Mutex<mpsc::UnboundedReceiver<Inbound>>,
    sent: mpsc::UnboundedSender<(Vec<u8>, SocketAddr)>,
}

impl RendezvousIo for FakeIo {
    async fn recv_from(&self, buffer: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let next = self.inbound.lock().await.recv().await;
        match next {
            Some(Ok((datagram, source))) => {
                let len = datagram.len().min(buffer.len());
                buffer[..len].copy_from_slice(&datagram[..len]);
                Ok((len, source))
            }
            Some(Err(error)) => Err(error),
            None => std::future::pending().await,
        }
    }

    async fn send_to(&self, datagram: &[u8], target: SocketAddr) -> io::Result<usize> {
        let _ = self.sent.send((datagram.to_vec(), target));
        Ok(datagram.len())
    }
}

struct Harness {
    control: Arc<ControlRegistry>,
    inbound: mpsc::UnboundedSender<Inbound>,
    sent: mpsc::UnboundedReceiver<(Vec<u8>, SocketAddr)>,
}

impl Harness {
    fn start() -> Self {
        let address = RendezvousAddress::parse("rendezvous.example.com:7777").unwrap();
        let control = Arc::new(ControlRegistry::new().with_udp_rendezvous(address));
        let (inbound, inbound_rx) = mpsc::unbounded_channel();
        let (sent_tx, sent) = mpsc::unbounded_channel();
        let io = FakeIo {
            inbound: tokio::sync::Mutex::new(inbound_rx),
            sent: sent_tx,
        };
        let serving = Arc::clone(&control);
        tokio::spawn(async move {
            serve_io(&io, serving.punches(), AddressPolicy::active()).await;
        });
        Self {
            control,
            inbound,
            sent,
        }
    }

    fn receive(&self, datagram: Vec<u8>, source: SocketAddr) {
        self.inbound.send(Ok((datagram, source))).unwrap();
    }

    async fn next_reply(&mut self) -> (ProbeDatagram, SocketAddr) {
        let (datagram, target) = self.sent.recv().await.unwrap();
        (ProbeDatagram::decode(&datagram).unwrap(), target)
    }
}

struct Keys {
    punch_id: PunchId,
    gateway: RendezvousKey,
    device: RendezvousKey,
}

/// Registers a capable gateway, forwards one offer and answers it, as the
/// control connection and the POST would.
async fn answered_punch(control: &ControlRegistry, node: &str) -> Keys {
    let (mut rx, token) = control
        .register(
            node,
            KEY,
            Some(DirectCapability {
                version: DIRECT_PROTOCOL_VERSION,
                udp: true,
            }),
        )
        .unwrap();
    let pending = control
        .offer_direct(
            OfferSource {
                relay_node_id: node,
                remote_api_key: KEY,
                client: None,
            },
            SealedCandidates {
                n: "nonce".into(),
                enc: "offer".into(),
            },
        )
        .unwrap();
    let Some(ControlSignal::DirectOffer {
        punch_id,
        register: Some(register),
        ..
    }) = rx.recv().await
    else {
        panic!("expected a DirectOffer with a rendezvous");
    };
    control.report_direct(
        node,
        token,
        ControlReport::DirectAnswer {
            punch_id,
            answer: SealedCandidates {
                n: "nonce".into(),
                enc: "answer".into(),
            },
        },
    );
    let OfferOutcome::Answered(response) = pending.outcome().await else {
        panic!("expected an answer");
    };
    Keys {
        punch_id,
        gateway: register.key,
        device: response.rendezvous.expect("device rendezvous").key,
    }
}

fn register(punch_id: PunchId, role: PunchRole, key: &RendezvousKey) -> Vec<u8> {
    key.register(punch_id, role).unwrap().encode()
}

fn v4(octets: [u8; 4], port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from(octets), port))
}

#[tokio::test]
async fn a_punch_hands_each_role_the_others_mapping_over_real_sockets() {
    let server = RendezvousServer::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let server_addr = server.local_addr().unwrap();
    let address = RendezvousAddress::parse(&server_addr.to_string()).unwrap();
    let control = Arc::new(ControlRegistry::new().with_udp_rendezvous(address));
    let keys = answered_punch(&control, NODE).await;
    tokio::spawn(server.serve(Arc::clone(&control)));

    let gateway = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let device = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut buffer = [0u8; REGISTER_DATAGRAM_LEN];
    let mut exchange = async |socket: &UdpSocket, datagram: Vec<u8>| {
        socket.send_to(&datagram, server_addr).await.unwrap();
        let (len, from) = socket.recv_from(&mut buffer).await.unwrap();
        assert_eq!(
            from, server_addr,
            "replies leave from the rendezvous address"
        );
        ProbeDatagram::decode(&buffer[..len]).unwrap()
    };
    let punch_id = keys.punch_id;
    assert_eq!(
        exchange(
            &gateway,
            register(punch_id, PunchRole::Gateway, &keys.gateway)
        )
        .await,
        keys.gateway.registered(punch_id).unwrap()
    );
    let SocketAddr::V4(gateway_mapping) = gateway.local_addr().unwrap() else {
        panic!("IPv4 socket");
    };
    let SocketAddr::V4(device_mapping) = device.local_addr().unwrap() else {
        panic!("IPv4 socket");
    };
    assert_eq!(
        exchange(&device, register(punch_id, PunchRole::Device, &keys.device)).await,
        keys.device.peer(punch_id, gateway_mapping).unwrap()
    );
    assert_eq!(
        exchange(
            &gateway,
            register(punch_id, PunchRole::Gateway, &keys.gateway)
        )
        .await,
        keys.gateway.peer(punch_id, device_mapping).unwrap()
    );
}

#[tokio::test]
async fn sources_the_rendezvous_cannot_observe_are_ignored() {
    let mut harness = Harness::start();
    let keys = answered_punch(&harness.control, NODE).await;
    let gateway = register(keys.punch_id, PunchRole::Gateway, &keys.gateway);

    harness.receive(
        gateway.clone(),
        SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1),
            4000,
            0,
            0,
        )),
    );
    harness.receive(gateway.clone(), v4([10, 0, 0, 1], 4000));
    harness.receive(gateway.clone(), v4([127, 0, 0, 1], 0));
    let mapped = SocketAddr::V6(SocketAddrV6::new(
        Ipv4Addr::new(127, 0, 0, 1).to_ipv6_mapped(),
        4000,
        0,
        0,
    ));
    harness.receive(gateway, mapped);
    assert_eq!(
        harness.next_reply().await,
        (keys.gateway.registered(keys.punch_id).unwrap(), mapped),
        "only the v4-mapped source is answered, at the address it came from"
    );

    harness.receive(
        register(keys.punch_id, PunchRole::Device, &keys.device),
        v4([127, 0, 0, 2], 5000),
    );
    assert_eq!(
        harness.next_reply().await.0,
        keys.device
            .peer(
                keys.punch_id,
                SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 4000)
            )
            .unwrap(),
        "the latched mapping is the canonical IPv4 address"
    );
}

#[tokio::test]
async fn unknown_punches_and_malformed_datagrams_get_no_reply() {
    let mut harness = Harness::start();
    let keys = answered_punch(&harness.control, NODE).await;
    let source = v4([127, 0, 0, 1], 4000);

    harness.receive(
        register(PunchId::generate(), PunchRole::Gateway, &keys.gateway),
        source,
    );
    let mut oversized = register(keys.punch_id, PunchRole::Gateway, &keys.gateway);
    oversized.push(0);
    harness.receive(oversized, source);
    harness.receive(b"GET / HTTP/1.1\r\n".to_vec(), source);
    harness.receive(
        keys.gateway.registered(keys.punch_id).unwrap().encode(),
        source,
    );
    harness.receive(
        register(
            keys.punch_id,
            PunchRole::Gateway,
            &RendezvousKey::generate(),
        ),
        source,
    );
    harness.receive(
        register(keys.punch_id, PunchRole::Gateway, &keys.gateway),
        source,
    );
    assert_eq!(
        harness.next_reply().await,
        (keys.gateway.registered(keys.punch_id).unwrap(), source),
        "the first reply answers the only valid Register"
    );
}

#[tokio::test]
async fn per_punch_the_rendezvous_emits_no_more_than_it_receives() {
    let mut harness = Harness::start();
    let keys = answered_punch(&harness.control, NODE).await;
    let sentinel = answered_punch(&harness.control, "node-2").await;
    let gateway = v4([127, 0, 0, 1], 4000);
    let device = v4([127, 0, 0, 2], 5000);
    let wrong = RendezvousKey::generate();

    let mut script = Vec::new();
    for round in 0..MAX_DATAGRAMS_PER_PUNCH {
        script.push((
            register(keys.punch_id, PunchRole::Gateway, &keys.gateway),
            gateway,
        ));
        script.push((register(keys.punch_id, PunchRole::Gateway, &wrong), gateway));
        script.push((
            register(keys.punch_id, PunchRole::Gateway, &keys.gateway),
            v4([127, 0, 0, 1], 4001),
        ));
        if round % 2 == 0 {
            script.push((
                register(keys.punch_id, PunchRole::Device, &keys.device),
                device,
            ));
        }
    }
    let received_datagrams = script.len();
    let received_bytes: usize = script.iter().map(|(datagram, _)| datagram.len()).sum();
    for (datagram, source) in script {
        harness.receive(datagram, source);
    }
    let sentinel_source = v4([127, 0, 0, 3], 6000);
    harness.receive(
        register(sentinel.punch_id, PunchRole::Gateway, &sentinel.gateway),
        sentinel_source,
    );

    let mut sent_datagrams = 0;
    let mut sent_bytes = 0;
    loop {
        let (datagram, target) = harness.sent.recv().await.unwrap();
        if target == sentinel_source {
            break;
        }
        assert!(
            target == gateway || target == device,
            "replies go only to a latched sender"
        );
        assert!(datagram.len() < REGISTER_DATAGRAM_LEN);
        sent_datagrams += 1;
        sent_bytes += datagram.len();
    }
    assert!(sent_datagrams <= received_datagrams);
    assert!(sent_bytes <= received_bytes);
    assert!(
        sent_datagrams <= usize::try_from(MAX_DATAGRAMS_PER_PUNCH).unwrap(),
        "the punch's datagram budget bounds its replies"
    );
    assert!(sent_datagrams > 0);
}

#[tokio::test(start_paused = true)]
async fn receive_errors_back_off_and_a_datagram_clears_the_streak() {
    let mut harness = Harness::start();
    let keys = answered_punch(&harness.control, NODE).await;
    let source = v4([127, 0, 0, 1], 4000);
    let started = Instant::now();
    for _ in 0..3 {
        harness
            .inbound
            .send(Err(io::Error::other("receive failed")))
            .unwrap();
    }
    harness.receive(
        register(keys.punch_id, PunchRole::Gateway, &keys.gateway),
        source,
    );
    harness.next_reply().await;
    let streak: Duration = (1..=3).map(socket_recv_backoff).sum();
    assert_eq!(started.elapsed(), streak);

    let resumed = Instant::now();
    harness
        .inbound
        .send(Err(io::Error::other("receive failed")))
        .unwrap();
    harness.receive(
        register(keys.punch_id, PunchRole::Gateway, &keys.gateway),
        source,
    );
    harness.next_reply().await;
    assert_eq!(
        resumed.elapsed(),
        socket_recv_backoff(1),
        "a received datagram resets the backoff"
    );
}

#[tokio::test(start_paused = true)]
async fn unusable_source_warnings_are_rate_limited() {
    let mut warnings = SourceWarnings::default();
    let source = v4([10, 0, 0, 1], 4000);
    warnings.note(SourceRejection::NotPublic, source);
    warnings.note(SourceRejection::NotPublic, source);
    warnings.note(SourceRejection::NotIpv4, source);
    assert_eq!(warnings.not_public.suppressed, 1);
    assert_eq!(warnings.not_ipv4.suppressed, 0);
    tokio::time::advance(UDP_SOURCE_WARN_INTERVAL).await;
    warnings.note(SourceRejection::NotPublic, source);
    assert_eq!(
        warnings.not_public.suppressed, 0,
        "the next interval warns again"
    );
}

#[test]
fn the_bind_address_must_be_ipv4() {
    assert_eq!(
        parse_bind_address("0.0.0.0:7777").unwrap(),
        DEFAULT_UDP_BIND_ADDR
    );
    assert!(matches!(
        parse_bind_address("[::]:7777"),
        Err(RendezvousStartError::BindNotIpv4 { .. })
    ));
    assert!(matches!(
        parse_bind_address("rendezvous:7777"),
        Err(RendezvousStartError::BindSyntax { .. })
    ));
}

#[test]
fn the_public_address_is_normalised_and_never_private() {
    assert_eq!(
        RendezvousAddress::parse("Rendezvous.Example.com:7777")
            .unwrap()
            .as_str(),
        "rendezvous.example.com:7777"
    );
    assert!(matches!(
        RendezvousAddress::parse("10.0.0.1:7777"),
        Err(RendezvousStartError::PublicAddress(_))
    ));
    assert!(matches!(
        RendezvousAddress::parse("rendezvous.example.com"),
        Err(RendezvousStartError::PublicAddress(_))
    ));
}
