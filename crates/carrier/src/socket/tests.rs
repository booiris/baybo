use std::future::poll_fn;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4};
use std::time::Duration;

use remote_host_protocol::relay::{PROBE_DATAGRAM_MAGIC, PUNCH_TAG_LEN, PunchId, PunchTag};
use tokio::net::UdpSocket;
use tokio::time::{Instant, timeout};

use super::*;
use crate::quic::QUIC_VERSION_1;

const LOOPBACK: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
const SOURCE: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5)), 4000);
const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));
const TEST_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_FIRST_BYTE: u8 = 0xe3;
const INITIAL_FIRST_BYTE: u8 = 0xc3;
const SHORT_HEADER_BYTE: u8 = 0x41;
/// A version no carrier endpoint speaks (a reserved "greasing" one).
const FOREIGN_VERSION: u32 = 0x1a2a_3a4a;
const DCID_LEN: u8 = 8;
const RECV_BUFFER_LEN: usize = 128;

fn peer(punch_id: PunchId) -> ProbeDatagram {
    ProbeDatagram::Peer {
        punch_id,
        srflx: SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, 9), 40_000),
    }
}

fn punch(seq: u16) -> ProbeDatagram {
    ProbeDatagram::Punch {
        seq,
        tag: PunchTag::from_bytes([0x5a; PUNCH_TAG_LEN]),
    }
}

/// A long-header QUIC segment of `len` bytes: `first`, `version`, then a
/// destination connection ID length of `dcid_len`, padded with that byte.
fn long_header(first: u8, version: u32, dcid_len: u8, len: usize) -> Vec<u8> {
    let mut segment = vec![first];
    segment.extend_from_slice(&version.to_be_bytes());
    segment.resize(len, dcid_len);
    segment
}

fn received(len: usize, stride: usize) -> RecvMeta {
    RecvMeta {
        addr: SOURCE,
        len,
        stride,
        ecn: None,
        dst_ip: Some(LOCAL),
    }
}

/// Runs the demux over one synthetic receive buffer.
fn split(storage: &mut [u8], meta: RecvMeta) -> (RecvMeta, Vec<ReceivedProbe>) {
    let mut metas = [meta];
    let mut bufs = [IoSliceMut::new(storage)];
    let mut delivered = Vec::new();
    split_batch(&mut bufs, &mut metas, 1, |probe| delivered.push(probe));
    (metas[0], delivered)
}

fn datagrams(delivered: &[ReceivedProbe]) -> Vec<ProbeDatagram> {
    delivered
        .iter()
        .map(|probe| probe.datagram.clone())
        .collect()
}

#[test]
fn a_gro_buffer_of_peer_and_registered_yields_both_datagrams() {
    let punch_id = PunchId::generate();
    let registered = ProbeDatagram::Registered { punch_id };
    let stride = peer(punch_id).encode().len();
    let mut storage = [peer(punch_id).encode(), registered.encode()].concat();
    let len = storage.len();

    let (meta, delivered) = split(&mut storage, received(len, stride));

    assert_eq!(datagrams(&delivered), vec![peer(punch_id), registered]);
    assert!(
        delivered
            .iter()
            .all(|probe| probe.source == SOURCE && probe.local_ip == Some(LOCAL))
    );
    assert_eq!(meta.len, 0);
}

#[test]
fn quic_segments_around_a_probe_are_packed_on_their_stride() {
    let punch_id = PunchId::generate();
    let stride = peer(punch_id).encode().len();
    let long_header = long_header(HANDSHAKE_FIRST_BYTE, QUIC_VERSION_1, DCID_LEN, stride);
    let short_header = vec![SHORT_HEADER_BYTE; stride / 2];
    let mut storage = [
        long_header.clone(),
        peer(punch_id).encode(),
        short_header.clone(),
    ]
    .concat();
    let len = storage.len();

    let (meta, delivered) = split(&mut storage, received(len, stride));

    assert_eq!(datagrams(&delivered), vec![peer(punch_id)]);
    assert_eq!(meta.stride, stride);
    assert_eq!(meta.len, long_header.len() + short_header.len());
    assert_eq!(
        storage[..meta.len],
        [long_header, short_header].concat()[..]
    );
}

#[test]
fn a_buffer_without_probe_datagrams_is_left_untouched() {
    let stride = 30;
    let original = [
        long_header(INITIAL_FIRST_BYTE, QUIC_VERSION_1, DCID_LEN, stride),
        vec![SHORT_HEADER_BYTE; stride],
    ]
    .concat();
    let mut storage = original.clone();

    let (meta, delivered) = split(&mut storage, received(original.len(), stride));

    assert!(delivered.is_empty());
    assert_eq!(meta.len, original.len());
    assert_eq!(storage, original);
}

#[test]
fn a_malformed_probe_datagram_is_dropped_and_never_reaches_quic() {
    let mut storage = vec![PROBE_DATAGRAM_MAGIC, 3, 0, 0];
    let len = storage.len();

    let (meta, delivered) = split(&mut storage, received(len, len));

    assert!(delivered.is_empty());
    assert_eq!(meta.len, 0);
}

#[test]
fn quic_packets_quinn_would_answer_unadmitted_never_reach_it() {
    let stride = 40;
    let kept = long_header(HANDSHAKE_FIRST_BYTE, QUIC_VERSION_1, DCID_LEN, stride);
    let mut storage = [
        long_header(INITIAL_FIRST_BYTE, FOREIGN_VERSION, DCID_LEN, stride),
        long_header(INITIAL_FIRST_BYTE, QUIC_VERSION_1, DCID_LEN - 1, stride),
        kept.clone(),
        long_header(HANDSHAKE_FIRST_BYTE, 0, DCID_LEN, stride),
    ]
    .concat();
    let len = storage.len();

    let (meta, delivered) = split(&mut storage, received(len, stride));

    assert!(delivered.is_empty());
    assert_eq!(storage[..meta.len], kept[..]);
}

#[test]
fn a_short_long_header_packet_never_reaches_quinn() {
    let mut storage = vec![INITIAL_FIRST_BYTE, 0, 0, 0];
    let len = storage.len();

    let (meta, delivered) = split(&mut storage, received(len, len));

    assert!(delivered.is_empty());
    assert_eq!(meta.len, 0);
}

#[test]
fn a_probe_from_an_ipv4_mapped_source_reports_ipv4() {
    let mut storage = punch(1).encode();
    let len = storage.len();
    let meta = RecvMeta {
        addr: SocketAddr::new(Ipv4Addr::new(203, 0, 113, 5).to_ipv6_mapped().into(), 4000),
        dst_ip: Some(Ipv4Addr::new(10, 0, 0, 2).to_ipv6_mapped().into()),
        ..received(len, len)
    };

    let (_, delivered) = split(&mut storage, meta);

    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].source, SOURCE);
    assert_eq!(delivered[0].local_ip, Some(LOCAL));
}

#[tokio::test]
async fn probe_datagrams_arrive_on_the_queue_and_leave_from_the_same_socket() {
    let (socket, mut probes) = DemuxSocket::bind(LOOPBACK).unwrap();
    let _endpoint = socket.quic_endpoint(None).unwrap();
    let local = socket.local_addr().unwrap();
    let peer = UdpSocket::bind(LOOPBACK).await.unwrap();
    let peer_addr = peer.local_addr().unwrap();

    peer.send_to(&punch(7).encode(), local).await.unwrap();
    let probe = timeout(TEST_TIMEOUT, probes.recv()).await.unwrap().unwrap();
    assert_eq!(probe.datagram, punch(7));
    assert_eq!(probe.source, peer_addr);

    socket.send_probe(&punch(8), peer_addr, None).await.unwrap();
    let mut buf = [0; RECV_BUFFER_LEN];
    let (len, from) = timeout(TEST_TIMEOUT, peer.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(buf[..len], punch(8).encode()[..]);
    assert_eq!(from, local);
}

#[tokio::test]
async fn a_probe_send_error_reaches_the_caller_while_a_quic_send_absorbs_it() {
    let (socket, _probes) = DemuxSocket::bind(LOOPBACK).unwrap();
    let port_zero = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

    assert!(socket.send_probe(&punch(1), port_zero, None).await.is_err());

    let contents = punch(1).encode();
    let transmit = Transmit {
        destination: port_zero,
        ecn: None,
        contents: &contents,
        segment_size: None,
        src_ip: None,
    };
    assert!(EndpointIo(socket).try_send(&transmit).is_ok());
}

#[tokio::test]
async fn an_ipv6_socket_leaves_ipv4_to_the_other_family() {
    let (socket, _probes) =
        DemuxSocket::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)).unwrap();
    assert!(socket2::SockRef::from(&socket.io).only_v6().unwrap());
}

#[tokio::test]
async fn a_receive_error_is_absorbed_and_never_handed_to_quinn() {
    let (socket, _probes) = DemuxSocket::bind(LOOPBACK).unwrap();

    for kind in [io::ErrorKind::WouldBlock, io::ErrorKind::ConnectionReset] {
        assert_eq!(socket.settle(Err(kind.into())), None);
        assert_eq!(socket.consecutive_recv_errors(), 0);
        assert!(socket.recv_errors.lock().resume.is_none());
    }

    assert_eq!(socket.settle(Err(io::Error::other("injected"))), None);
    assert_eq!(socket.consecutive_recv_errors(), 1);
    assert!(socket.recv_errors.lock().resume.is_some());

    assert_eq!(socket.settle(Ok(3)), Some(3));
    assert_eq!(socket.consecutive_recv_errors(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_receive_error_backs_off_and_the_next_receive_clears_the_streak() {
    let (socket, mut probes) = DemuxSocket::bind(LOOPBACK).unwrap();
    for _ in 0..2 {
        assert_eq!(socket.settle(Err(io::Error::other("injected"))), None);
    }
    assert_eq!(socket.consecutive_recv_errors(), 2);

    let peer = UdpSocket::bind(LOOPBACK).await.unwrap();
    peer.send_to(&punch(3).encode(), socket.local_addr().unwrap())
        .await
        .unwrap();

    let started = Instant::now();
    let mut storage = [0; RECV_BUFFER_LEN];
    let mut meta = [RecvMeta::default()];
    let endpoint_io = EndpointIo(Arc::clone(&socket));
    let count = poll_fn(|cx| {
        let mut bufs = [IoSliceMut::new(&mut storage)];
        endpoint_io.poll_recv(cx, &mut bufs, &mut meta)
    })
    .await
    .unwrap();

    assert!(started.elapsed() >= socket_recv_backoff(2));
    assert_eq!(count, 1);
    assert_eq!(meta[0].len, 0);
    assert_eq!(socket.consecutive_recv_errors(), 0);
    assert_eq!(probes.try_recv().unwrap().datagram, punch(3));
}

#[tokio::test]
async fn injected_receive_errors_count_and_back_off_like_real_ones() {
    let (socket, _probes) = DemuxSocket::bind(LOOPBACK).unwrap();
    socket.inject_recv_errors(3);
    assert_eq!(socket.consecutive_recv_errors(), 3);
    assert!(socket.recv_errors.lock().resume.is_some());
    assert_eq!(socket.settle(Ok(1)), Some(1));
    assert_eq!(socket.consecutive_recv_errors(), 0);
}

#[tokio::test]
async fn a_socket_carries_one_endpoint() {
    let (socket, _probes) = DemuxSocket::bind(LOOPBACK).unwrap();
    let _endpoint = socket.quic_endpoint(None).unwrap();
    assert!(matches!(
        socket.quic_endpoint(None),
        Err(CarrierError::Endpoint { .. })
    ));
}
