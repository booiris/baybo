//! The phone's end of a direct carrier, as the carrier runtime's tests and the
//! relay E2E drive it: a probe socket with its QUIC client endpoint, and
//! sessions opened with `DirectOpen`, Noise IK and its confirmation over a
//! QUIC stream. Also the bounded waits those tests share.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use carrier::framing::{FrameReader, write_direct_open, write_frame};
use carrier::quic::{DIRECT_QUIC_SERVER_NAME, client_config};
use carrier::socket::{DemuxSocket, ProbeReceiver};
use device_proto::aead::KEY_LEN;
use device_proto::api_tunnel::{self, TunnelRequest, TunnelResponse};
use device_proto::candidates::CertHash;
use device_proto::noise::{FrameReassembler, NOISE_MAX_MESSAGE, StaticKeypair, write_chunked};
use remote_host_protocol::relay::{DirectOpen, DirectToken, LegClass};
use snow::TransportState;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::timeout;

/// How long one step of a carrier test may take before the test fails.
pub(crate) const STEP: Duration = Duration::from_secs(10);
/// How long a carrier test listens before it concludes nothing answers.
pub(crate) const QUIET: Duration = Duration::from_millis(600);
const POLL: Duration = Duration::from_millis(5);

/// Polls `condition` until it holds, failing the test after [`STEP`].
pub(crate) async fn until(what: &str, mut condition: impl FnMut() -> bool) {
    timeout(STEP, async {
        while !condition() {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
}

/// Whether this host can bind UDP on `::1`. The IPv6-loopback carrier tests
/// skip themselves without it, as a host with IPv6 disabled has none.
pub(crate) fn ipv6_loopback_available() -> bool {
    std::net::UdpSocket::bind((std::net::Ipv6Addr::LOCALHOST, 0)).is_ok()
}

/// The QUIC application error the gateway closed `connection` with.
pub(crate) async fn close_code(connection: &quinn::Connection) -> quinn::VarInt {
    match timeout(STEP, connection.closed())
        .await
        .expect("the gateway closes the connection")
    {
        quinn::ConnectionError::ApplicationClosed(close) => close.error_code,
        other => panic!("closed without an application code: {other:?}"),
    }
}

/// The phone's probe socket and its client endpoint.
pub(crate) struct Phone {
    pub(crate) socket: Arc<DemuxSocket>,
    pub(crate) endpoint: quinn::Endpoint,
    /// Every probe datagram the socket receives: rendezvous replies and the
    /// gateway's punches.
    pub(crate) probes: ProbeReceiver,
}

impl Phone {
    pub(crate) fn bind(address: SocketAddr) -> Self {
        let (socket, probes) = DemuxSocket::bind(address).unwrap();
        let endpoint = socket.quic_endpoint(None).unwrap();
        Self {
            socket,
            endpoint,
            probes,
        }
    }

    pub(crate) fn address(&self) -> SocketAddr {
        self.socket.local_addr().unwrap()
    }

    pub(crate) fn connecting(&self, pinned: CertHash, target: SocketAddr) -> quinn::Connecting {
        let client = client_config(
            pinned,
            Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
        )
        .unwrap();
        self.endpoint
            .connect_with(client, target, DIRECT_QUIC_SERVER_NAME)
            .unwrap()
    }

    pub(crate) async fn connect(&self, pinned: CertHash, target: SocketAddr) -> quinn::Connection {
        timeout(STEP, self.connecting(pinned, target))
            .await
            .expect("the gateway admits the phone")
            .expect("the QUIC handshake completes")
    }

    /// Whether a connect to `target` gets no reply at all for [`QUIET`].
    pub(crate) async fn is_ignored(&self, pinned: CertHash, target: SocketAddr) -> bool {
        timeout(QUIET, self.connecting(pinned, target))
            .await
            .is_err()
    }
}

/// The paired device's static key and the gateway static key it pins, which
/// every carrier session's Noise IK handshake runs between.
#[derive(Clone, Copy)]
pub(crate) struct PhoneKeys<'a> {
    pub(crate) device: &'a StaticKeypair,
    pub(crate) gateway_public: &'a [u8; KEY_LEN],
}

impl PhoneKeys<'_> {
    /// Opens a session of `class` on `connection`: the `DirectOpen` preface,
    /// then the Noise IK initiator and its confirmation.
    pub(crate) async fn session(
        self,
        connection: &quinn::Connection,
        token: &DirectToken,
        class: LegClass,
    ) -> QuicSession {
        let (send, recv) = connection.open_bi().await.unwrap();
        self.open_session(send, recv, token, class).await
    }

    async fn open_session<W, R>(
        self,
        mut send: W,
        recv: R,
        token: &DirectToken,
        class: LegClass,
    ) -> PhoneSession<W, R>
    where
        W: AsyncWrite + Unpin,
        R: AsyncRead + Unpin,
    {
        write_direct_open(
            &mut send,
            &DirectOpen {
                token: token.clone(),
                class,
            },
        )
        .await
        .unwrap();
        let mut frames = FrameReader::new(recv);
        let transport = self.noise(&mut send, &mut frames).await;
        PhoneSession {
            send,
            frames,
            transport,
            reassembler: FrameReassembler::new(),
            received: VecDeque::new(),
        }
    }

    async fn noise<W, R>(self, send: &mut W, frames: &mut FrameReader<R>) -> TransportState
    where
        W: AsyncWrite + Unpin,
        R: AsyncRead + Unpin,
    {
        let mut handshake = self.device.ik_initiator(self.gateway_public).unwrap();
        let mut buffer = vec![0u8; NOISE_MAX_MESSAGE];
        let len = handshake.write_message(&[], &mut buffer).unwrap();
        write_frame(send, &buffer[..len]).await.unwrap();
        let reply = timeout(STEP, frames.next_frame())
            .await
            .expect("the gateway answers the handshake")
            .unwrap()
            .expect("handshake message 2");
        handshake.read_message(&reply, &mut buffer).unwrap();
        let mut transport = handshake.into_transport_mode().unwrap();
        let len = transport.write_message(&[], &mut buffer).unwrap();
        write_frame(send, &buffer[..len]).await.unwrap();
        transport
    }
}

/// The phone's end of one carrier session.
pub(crate) struct PhoneSession<W, R> {
    send: W,
    frames: FrameReader<R>,
    transport: TransportState,
    reassembler: FrameReassembler,
    received: VecDeque<Vec<u8>>,
}

pub(crate) type QuicSession = PhoneSession<quinn::SendStream, quinn::RecvStream>;

impl<W, R> PhoneSession<W, R>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    /// Seals `plaintext` and sends it in as many transport messages as it
    /// takes, one frame each.
    pub(crate) async fn send_plaintext(&mut self, plaintext: &[u8]) {
        for message in write_chunked(&mut self.transport, plaintext).unwrap() {
            write_frame(&mut self.send, &message).await.unwrap();
        }
    }

    /// The next plaintext the gateway sent, reassembled from its chunks.
    pub(crate) async fn recv_plaintext(&mut self) -> Vec<u8> {
        while self.received.is_empty() {
            let message = timeout(STEP, self.frames.next_frame())
                .await
                .expect("the gateway answers")
                .unwrap()
                .expect("a transport message");
            self.received.extend(
                self.reassembler
                    .read(&mut self.transport, &message)
                    .unwrap(),
            );
        }
        self.received.pop_front().unwrap()
    }

    pub(crate) async fn send_tunnel(&mut self, request: &TunnelRequest) {
        self.send_plaintext(&api_tunnel::encode(request).unwrap())
            .await;
    }

    /// The next tunnel response message from the gateway.
    pub(crate) async fn recv_tunnel(&mut self) -> TunnelResponse {
        api_tunnel::decode(&self.recv_plaintext().await).unwrap()
    }

    /// Resolves when the gateway's end of the stream is gone.
    pub(crate) async fn ended(&mut self) {
        timeout(STEP, async {
            while let Ok(Some(_)) = self.frames.next_frame().await {}
        })
        .await
        .expect("the gateway ends the session");
    }
}
