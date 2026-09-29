use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use futures::FutureExt;
use parking_lot::Mutex;
use quinn::crypto::{Keys, Session, UnsupportedVersion};
use quinn::{ConnectError, Connection, ConnectionError, ConnectionId, Endpoint};
use quinn_proto::transport_parameters::TransportParameters;
use remote_host_protocol::relay::{
    DirectOpen, DirectToken, LegClass, PUNCH_TAG_LEN, ProbeDatagram, PunchTag,
};
use tokio::net::UdpSocket;

use super::*;
use crate::framing::{FrameReader, read_direct_open, write_direct_open, write_frame};
use crate::socket::DemuxSocket;

const LOOPBACK: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
const DISCARD: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9);
const TEST_TIMEOUT: Duration = Duration::from_secs(10);
/// RFC 9287's transport parameter id.
const GREASE_QUIC_BIT_PARAMETER: u64 = 0x2ab2;
/// Long enough for any reply to arrive over loopback; a reply that did would
/// fail the test, so a loaded runner can only make it pass vacuously, never
/// fail spuriously.
const QUIET_WINDOW: Duration = Duration::from_millis(200);
/// RFC 9000 §14.1: a server ignores a client Initial in a smaller datagram.
const MIN_INITIAL_DATAGRAM_LEN: usize = 1200;
const RECV_BUFFER_LEN: usize = 2048;

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

fn endpoint(server: Option<quinn::ServerConfig>) -> Endpoint {
    let (socket, _probes) = DemuxSocket::bind(LOOPBACK).unwrap();
    socket.quic_endpoint(server).unwrap()
}

/// One connection attempt: the client's connect and the server's first accept.
async fn handshake(
    server: &Endpoint,
    client: &Endpoint,
    config: quinn::ClientConfig,
) -> (
    Result<Connection, ConnectionError>,
    Result<Connection, ConnectionError>,
) {
    let connecting = client
        .connect_with(
            config,
            server.local_addr().unwrap(),
            DIRECT_QUIC_SERVER_NAME,
        )
        .unwrap();
    let accepting = async { server.accept().await.unwrap().await };
    tokio::time::timeout(TEST_TIMEOUT, async { tokio::join!(connecting, accepting) })
        .await
        .unwrap()
}

async fn connected_pair(identity: &ServerIdentity) -> (Endpoint, Endpoint, Connection, Connection) {
    let server = endpoint(Some(server_config(identity, provider()).unwrap()));
    let client = endpoint(None);
    let config = client_config(identity.cert_hash(), provider()).unwrap();
    let (client_conn, server_conn) = handshake(&server, &client, config).await;
    (server, client, client_conn.unwrap(), server_conn.unwrap())
}

fn negotiated_alpn(connection: &Connection) -> Option<Vec<u8>> {
    connection
        .handshake_data()?
        .downcast::<quinn::crypto::rustls::HandshakeData>()
        .ok()?
        .protocol
}

#[tokio::test]
async fn a_pinned_handshake_carries_a_framed_stream() {
    let identity = ServerIdentity::generate().unwrap();
    let (_server, _client, client_conn, server_conn) = connected_pair(&identity).await;
    assert_eq!(
        negotiated_alpn(&client_conn),
        Some(DIRECT_QUIC_ALPN.as_bytes().to_vec())
    );

    let open = DirectOpen {
        token: DirectToken::generate(),
        class: LegClass::Api,
    };
    let (mut send, _recv) = client_conn.open_bi().await.unwrap();
    write_direct_open(&mut send, &open).await.unwrap();
    write_frame(&mut send, b"noise").await.unwrap();

    let (_send, recv) = tokio::time::timeout(TEST_TIMEOUT, server_conn.accept_bi())
        .await
        .unwrap()
        .unwrap();
    let mut reader = FrameReader::new(recv);
    assert_eq!(read_direct_open(&mut reader).await.unwrap(), open);
    assert_eq!(reader.next_frame().await.unwrap().unwrap(), b"noise");
}

#[tokio::test]
async fn a_different_self_signed_certificate_fails_the_handshake_before_any_stream() {
    let presented = ServerIdentity::generate().unwrap();
    let pinned = ServerIdentity::generate().unwrap();
    let server = endpoint(Some(server_config(&presented, provider()).unwrap()));
    let client = endpoint(None);
    let config = client_config(pinned.cert_hash(), provider()).unwrap();

    let (client_conn, server_conn) = handshake(&server, &client, config).await;

    assert!(client_conn.is_err());
    assert!(server_conn.is_err());
}

#[tokio::test]
async fn the_server_grants_bidi_streams_only_and_the_client_grants_none() {
    let identity = ServerIdentity::generate().unwrap();
    let (_server, _client, client_conn, server_conn) = connected_pair(&identity).await;

    let granted: Vec<_> = (0..MAX_STREAMS_PER_CONNECTION)
        .map_while(|_| client_conn.open_bi().now_or_never())
        .collect();
    assert_eq!(
        granted.len(),
        usize::try_from(MAX_STREAMS_PER_CONNECTION).unwrap()
    );
    assert!(client_conn.open_bi().now_or_never().is_none());
    assert!(client_conn.open_uni().now_or_never().is_none());
    assert!(client_conn.max_datagram_size().is_none());

    assert!(server_conn.open_bi().now_or_never().is_none());
    assert!(server_conn.open_uni().now_or_never().is_none());
    assert!(server_conn.max_datagram_size().is_none());
}

#[tokio::test]
async fn no_carrier_endpoint_advertises_the_grease_quic_bit() {
    let identity = ServerIdentity::generate().unwrap();
    let server_sent = Arc::new(Advertised::default());
    let client_sent = Arc::new(Advertised::default());
    let server_crypto = RecordingServer {
        inner: Arc::new(server_crypto(&identity, provider()).unwrap()),
        advertised: Arc::clone(&server_sent),
    };
    let client_crypto = RecordingClient {
        inner: Arc::new(client_crypto(identity.cert_hash(), provider()).unwrap()),
        advertised: Arc::clone(&client_sent),
    };
    let server = endpoint(Some(server_config_with(Arc::new(server_crypto))));
    let client = endpoint(None);

    let (client_conn, server_conn) = handshake(
        &server,
        &client,
        client_config_with(Arc::new(client_crypto)),
    )
    .await;
    client_conn.unwrap();
    server_conn.unwrap();

    for sent in [server_sent.sessions(), client_sent.sessions()] {
        assert_eq!(sent.len(), 1);
        assert!(!sent[0].is_empty());
        assert!(!sent[0].contains(&GREASE_QUIC_BIT_PARAMETER));
    }
}

/// A quinn endpoint with quinn's default configuration on a plain socket.
fn plain_endpoint(server: Option<quinn::ServerConfig>) -> Endpoint {
    Endpoint::new(
        EndpointConfig::default(),
        server,
        std::net::UdpSocket::bind(LOOPBACK).unwrap(),
        Arc::new(quinn::TokioRuntime),
    )
    .unwrap()
}

#[tokio::test]
async fn quinns_default_endpoint_config_would_advertise_the_grease_quic_bit() {
    let identity = ServerIdentity::generate().unwrap();
    let default = plain_endpoint(None);
    let sent = Arc::new(Advertised::default());
    let crypto = RecordingClient {
        inner: Arc::new(client_crypto(identity.cert_hash(), provider()).unwrap()),
        advertised: Arc::clone(&sent),
    };

    let _connecting = default
        .connect_with(
            client_config_with(Arc::new(crypto)),
            DISCARD,
            DIRECT_QUIC_SERVER_NAME,
        )
        .unwrap();

    assert!(sent.sessions()[0].contains(&GREASE_QUIC_BIT_PARAMETER));
}

/// Datagrams quinn's server answers before an `Incoming` exists: one naming an
/// unsupported version (answered with Version Negotiation) and a v1 Initial
/// whose destination connection ID is 4 bytes (answered with CONNECTION_CLOSE).
fn pre_admission_triggers() -> [Vec<u8>; 2] {
    let unknown_version = vec![0xc0, 0x1a, 0x2a, 0x3a, 0x4a, 0, 0];
    // first byte, version 1, DCID length 4, DCID, SCID length 0, token length 0
    let mut short_dcid_initial = vec![0xc0, 0, 0, 0, 1, 4, 1, 2, 3, 4, 0, 0];
    let payload_len = MIN_INITIAL_DATAGRAM_LEN - short_dcid_initial.len() - size_of::<u16>();
    let payload_len = u16::try_from(payload_len).unwrap();
    short_dcid_initial.extend_from_slice(&(0x4000 | payload_len).to_be_bytes());
    short_dcid_initial.resize(MIN_INITIAL_DATAGRAM_LEN, 0);
    [unknown_version, short_dcid_initial]
}

#[tokio::test]
async fn quinn_alone_answers_an_unadmitted_source_before_any_incoming() {
    let identity = ServerIdentity::generate().unwrap();
    let server = plain_endpoint(Some(server_config(&identity, provider()).unwrap()));
    let peer = UdpSocket::bind(LOOPBACK).await.unwrap();

    for trigger in pre_admission_triggers() {
        peer.send_to(&trigger, server.local_addr().unwrap())
            .await
            .unwrap();
        let mut reply = [0; RECV_BUFFER_LEN];
        tokio::time::timeout(TEST_TIMEOUT, peer.recv_from(&mut reply))
            .await
            .unwrap()
            .unwrap();
    }
    assert!(server.accept().now_or_never().is_none());
}

#[tokio::test]
async fn a_carrier_server_answers_no_unadmitted_source() {
    let identity = ServerIdentity::generate().unwrap();
    let (socket, mut probes) = DemuxSocket::bind(LOOPBACK).unwrap();
    let _server = socket
        .quic_endpoint(Some(server_config(&identity, provider()).unwrap()))
        .unwrap();
    let local = socket.local_addr().unwrap();
    let peer = UdpSocket::bind(LOOPBACK).await.unwrap();
    let marker = ProbeDatagram::Punch {
        seq: 1,
        tag: PunchTag::from_bytes([0; PUNCH_TAG_LEN]),
    };

    for trigger in pre_admission_triggers() {
        peer.send_to(&trigger, local).await.unwrap();
    }
    peer.send_to(&marker.encode(), local).await.unwrap();
    let read = tokio::time::timeout(TEST_TIMEOUT, probes.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.datagram, marker);

    let mut reply = [0; RECV_BUFFER_LEN];
    assert!(
        tokio::time::timeout(QUIET_WINDOW, peer.recv_from(&mut reply))
            .await
            .is_err()
    );
}

#[test]
fn the_verifier_accepts_only_the_pinned_certificate_presented_alone() {
    let identity = ServerIdentity::generate().unwrap();
    let other = ServerIdentity::generate().unwrap();
    let verifier = PinnedServerCert {
        pinned: identity.cert_hash(),
        provider: provider(),
    };
    let verify = |end_entity: &CertificateDer<'_>, intermediates: &[CertificateDer<'_>]| {
        let name = ServerName::try_from(DIRECT_QUIC_SERVER_NAME).unwrap();
        verifier.verify_server_cert(end_entity, intermediates, &name, &[], UnixTime::now())
    };

    assert!(verify(&identity.certificate, &[]).is_ok());
    assert!(verify(&other.certificate, &[]).is_err());
    assert!(
        verify(
            &identity.certificate,
            std::slice::from_ref(&other.certificate)
        )
        .is_err()
    );
}

#[test]
fn a_server_identity_prints_no_key() {
    let identity = ServerIdentity::generate().unwrap();
    let printed = format!("{identity:?}");
    assert!(printed.contains("<redacted>"));
    assert_eq!(
        identity.cert_hash(),
        CertHash::of_certificate(&identity.certificate)
    );
}

/// The transport parameter ids each session of one side advertised.
#[derive(Default)]
struct Advertised(Mutex<Vec<Vec<u64>>>);

impl Advertised {
    fn record(&self, params: &TransportParameters) {
        let mut encoded = Vec::new();
        params.write(&mut encoded);
        self.0.lock().push(parameter_ids(&encoded));
    }

    fn sessions(&self) -> Vec<Vec<u64>> {
        self.0.lock().clone()
    }
}

/// Ids of an encoded transport parameter list: `(id, len, value)` triples,
/// with `id` and `len` as QUIC variable-length integers.
fn parameter_ids(mut encoded: &[u8]) -> Vec<u64> {
    let mut ids = Vec::new();
    while !encoded.is_empty() {
        ids.push(read_varint(&mut encoded));
        let len = usize::try_from(read_varint(&mut encoded)).unwrap();
        encoded = &encoded[len..];
    }
    ids
}

fn read_varint(input: &mut &[u8]) -> u64 {
    let len = 1 << (input[0] >> 6);
    let value = input[1..len]
        .iter()
        .fold(u64::from(input[0] & 0x3f), |value, byte| {
            (value << 8) | u64::from(*byte)
        });
    *input = &input[len..];
    value
}

struct RecordingServer {
    inner: Arc<QuicServerConfig>,
    advertised: Arc<Advertised>,
}

impl quinn::crypto::ServerConfig for RecordingServer {
    fn initial_keys(
        &self,
        version: u32,
        dst_cid: &ConnectionId,
    ) -> Result<Keys, UnsupportedVersion> {
        self.inner.initial_keys(version, dst_cid)
    }

    fn retry_tag(&self, version: u32, orig_dst_cid: &ConnectionId, packet: &[u8]) -> [u8; 16] {
        self.inner.retry_tag(version, orig_dst_cid, packet)
    }

    fn start_session(
        self: Arc<Self>,
        version: u32,
        params: &TransportParameters,
    ) -> Box<dyn Session> {
        self.advertised.record(params);
        Arc::clone(&self.inner).start_session(version, params)
    }
}

struct RecordingClient {
    inner: Arc<QuicClientConfig>,
    advertised: Arc<Advertised>,
}

impl quinn::crypto::ClientConfig for RecordingClient {
    fn start_session(
        self: Arc<Self>,
        version: u32,
        server_name: &str,
        params: &TransportParameters,
    ) -> Result<Box<dyn Session>, ConnectError> {
        self.advertised.record(params);
        Arc::clone(&self.inner).start_session(version, server_name, params)
    }
}
