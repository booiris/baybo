//! `DemuxSocket`: a side's one UDP socket per address family, shared by its
//! QUIC endpoint, its UDP rendezvous registration and its punches, so the IPv4
//! mapping C observes is the mapping QUIC uses.

use std::fmt;
use std::future::Future;
use std::io::{self, IoSliceMut};
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use parking_lot::Mutex;
use quinn::udp::{RecvMeta, Transmit, UdpSocketState};
use quinn::{AsyncUdpSocket, UdpPoller};
use remote_host_protocol::relay::{
    AddressPolicy, ProbeDatagram, is_probe_datagram, socket_recv_backoff,
};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::Interest;
use tokio::sync::mpsc;
use tokio::time::Sleep;

use crate::error::CarrierError;
use crate::quic::{endpoint_config, reaches_endpoint};

/// Probe datagrams received but not yet taken by the socket's owner. More are
/// dropped, as UDP loss would drop them.
pub const PROBE_QUEUE_CAPACITY: usize = 256;

/// A decoded probe datagram and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedProbe {
    pub datagram: ProbeDatagram,
    /// The sender, with an IPv4-mapped address canonicalised to IPv4.
    pub source: SocketAddr,
    /// The local address the sender targeted, where the platform reports it.
    pub local_ip: Option<IpAddr>,
}

pub type ProbeReceiver = mpsc::Receiver<ReceivedProbe>;

/// A UDP socket built on quinn-udp's `UdpSocketState`, so a reply leaves from
/// the local address the peer targeted. Each received segment whose first byte
/// has neither bit of `QUIC_FIRST_BYTE_MASK` set is a probe datagram: it is
/// decoded and queued for the owner, never handed to quinn.
///
/// The socket carries exactly one QUIC endpoint, and that endpoint's driver is
/// the only reader, so probe datagrams arrive once
/// [`DemuxSocket::quic_endpoint`] has run. Only that method hands the socket
/// to quinn.
pub struct DemuxSocket {
    io: tokio::net::UdpSocket,
    state: UdpSocketState,
    probes: mpsc::Sender<ReceivedProbe>,
    recv_errors: Mutex<RecvErrors>,
    has_endpoint: Mutex<bool>,
}

/// A receive error returned to quinn would end its endpoint driver, and with
/// it every connection on the socket, so the demux absorbs it and backs off.
#[derive(Default)]
struct RecvErrors {
    consecutive: u32,
    resume: Option<Pin<Box<Sleep>>>,
}

impl DemuxSocket {
    /// Binds `address`, an IPv6 one with `IPV6_V6ONLY` so the family's IPv4
    /// socket owns IPv4, and returns the socket with the queue its probe
    /// datagrams arrive on. Must run inside a Tokio runtime.
    pub fn bind(address: SocketAddr) -> Result<(Arc<Self>, ProbeReceiver), CarrierError> {
        let bind_error = |error: io::Error| CarrierError::Bind {
            address,
            reason: error.to_string(),
        };
        tokio::runtime::Handle::try_current().map_err(|error| CarrierError::Bind {
            address,
            reason: error.to_string(),
        })?;
        let socket = bind_std(address).map_err(bind_error)?;
        let state = UdpSocketState::new((&socket).into()).map_err(bind_error)?;
        let io = tokio::net::UdpSocket::from_std(socket).map_err(bind_error)?;
        let (probes, receiver) = mpsc::channel(PROBE_QUEUE_CAPACITY);
        let socket = Self {
            io,
            state,
            probes,
            recv_errors: Mutex::default(),
            has_endpoint: Mutex::new(false),
        };
        Ok((Arc::new(socket), receiver))
    }

    /// The socket's QUIC endpoint: A passes its server configuration, P
    /// `None`. Every carrier endpoint is built here, so every one has
    /// `grease_quic_bit` off. A second call fails, since two endpoints would
    /// split the socket's datagrams between them.
    pub fn quic_endpoint(
        self: &Arc<Self>,
        server: Option<quinn::ServerConfig>,
    ) -> Result<quinn::Endpoint, CarrierError> {
        let mut has_endpoint = self.has_endpoint.lock();
        if *has_endpoint {
            return Err(CarrierError::Endpoint {
                reason: "the socket already carries an endpoint".to_owned(),
            });
        }
        let endpoint = quinn::Endpoint::new_with_abstract_socket(
            endpoint_config(),
            server,
            Arc::new(EndpointIo(Arc::clone(self))),
            Arc::new(quinn::TokioRuntime),
        )
        .map_err(|error| CarrierError::Endpoint {
            reason: error.to_string(),
        })?;
        *has_endpoint = true;
        Ok(endpoint)
    }

    /// Sends one probe datagram to `destination`, from `source` when given.
    /// Unlike a QUIC transmit, whose send errors quinn-udp logs and drops, a
    /// send error here reaches the caller.
    pub async fn send_probe(
        &self,
        datagram: &ProbeDatagram,
        destination: SocketAddr,
        source: Option<IpAddr>,
    ) -> io::Result<()> {
        let contents = datagram.encode();
        let transmit = Transmit {
            destination,
            ecn: None,
            contents: &contents,
            segment_size: None,
            src_ip: source,
        };
        self.io
            .async_io(Interest::WRITABLE, || {
                self.state.try_send((&self.io).into(), &transmit)
            })
            .await
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.io.local_addr()
    }

    /// Receive errors in a row since the last successful receive. The owner
    /// rebinds the socket when the streak persists.
    pub fn consecutive_recv_errors(&self) -> u32 {
        self.recv_errors.lock().consecutive
    }

    /// Records `count` failed receives, backing off after each exactly as a
    /// real one does: a real socket cannot be made to fail its receives.
    #[cfg(any(test, feature = "test-support"))]
    pub fn inject_recv_errors(&self, count: u32) {
        let error = io::Error::other("injected receive error");
        for _ in 0..count {
            self.back_off(&error);
        }
    }

    /// Settles one receive attempt: the batch count to hand quinn, or `None`
    /// to read again. An error never reaches quinn.
    fn settle(&self, received: io::Result<usize>) -> Option<usize> {
        match received {
            Ok(count) => {
                self.recovered();
                Some(count)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::ConnectionReset
                ) =>
            {
                None
            }
            Err(error) => {
                self.back_off(&error);
                None
            }
        }
    }

    fn poll_backoff(&self, cx: &mut Context<'_>) -> Poll<()> {
        let mut errors = self.recv_errors.lock();
        if let Some(resume) = errors.resume.as_mut() {
            ready!(resume.as_mut().poll(cx));
            errors.resume = None;
        }
        Poll::Ready(())
    }

    fn back_off(&self, error: &io::Error) {
        let mut errors = self.recv_errors.lock();
        errors.consecutive = errors.consecutive.saturating_add(1);
        let delay = socket_recv_backoff(errors.consecutive);
        if errors.consecutive == 1 {
            tracing::warn!(%error, ?delay, "direct-carrier: UDP receive failed; backing off");
        } else {
            tracing::debug!(
                %error,
                consecutive = errors.consecutive,
                ?delay,
                "direct-carrier: UDP receive still failing"
            );
        }
        errors.resume = Some(Box::pin(tokio::time::sleep(delay)));
    }

    fn recovered(&self) {
        let mut errors = self.recv_errors.lock();
        if errors.consecutive > 0 {
            tracing::info!(
                after = errors.consecutive,
                "direct-carrier: UDP receive recovered"
            );
            errors.consecutive = 0;
        }
    }

    fn deliver(&self, probe: ReceivedProbe) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.probes.try_send(probe) {
            tracing::debug!("direct-carrier: probe queue full; dropped a probe datagram");
        }
    }
}

impl fmt::Debug for DemuxSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DemuxSocket")
            .field("io", &self.io)
            .finish_non_exhaustive()
    }
}

/// The endpoint's side of a [`DemuxSocket`]. Only
/// [`DemuxSocket::quic_endpoint`] builds one, so no endpoint on a carrier
/// socket can skip `endpoint_config` or share the socket with a second reader.
#[derive(Debug)]
struct EndpointIo(Arc<DemuxSocket>);

impl AsyncUdpSocket for EndpointIo {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(WritablePoller {
            socket: Arc::clone(&self.0),
            waiting: None,
        })
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        let socket = &self.0;
        socket.io.try_io(Interest::WRITABLE, || {
            socket.state.send((&socket.io).into(), transmit)
        })
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let socket = &self.0;
        loop {
            ready!(socket.poll_backoff(cx));
            let received = ready!(socket.io.poll_recv_ready(cx)).and_then(|()| {
                socket.io.try_io(Interest::READABLE, || {
                    socket.state.recv((&socket.io).into(), bufs, meta)
                })
            });
            if let Some(count) = socket.settle(received) {
                split_batch(bufs, meta, count, |probe| socket.deliver(probe));
                return Poll::Ready(Ok(count));
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.io.local_addr()
    }

    fn may_fragment(&self) -> bool {
        self.0.state.may_fragment()
    }

    fn max_transmit_segments(&self) -> usize {
        self.0.state.max_gso_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.0.state.gro_segments()
    }
}

/// Waits for write readiness with its own waker, so each of quinn's pollers
/// on the socket is woken independently.
struct WritablePoller {
    socket: Arc<DemuxSocket>,
    waiting: Option<Pin<Box<dyn Future<Output = io::Result<()>> + Send + Sync>>>,
}

impl UdpPoller for WritablePoller {
    fn poll_writable(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        let this = &mut *self;
        let waiting = this.waiting.get_or_insert_with(|| {
            let socket = Arc::clone(&this.socket);
            Box::pin(async move { socket.io.writable().await })
        });
        let writable = waiting.as_mut().poll(cx);
        if writable.is_ready() {
            this.waiting = None;
        }
        writable
    }
}

impl fmt::Debug for WritablePoller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WritablePoller").finish_non_exhaustive()
    }
}

fn bind_std(address: SocketAddr) -> io::Result<std::net::UdpSocket> {
    let socket = Socket::new(
        Domain::for_address(address),
        Type::DGRAM,
        Some(Protocol::UDP),
    )?;
    if address.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.set_nonblocking(true)?;
    socket.bind(&address.into())?;
    Ok(socket.into())
}

/// Demultiplexes one receive batch in place. Each buffer may hold several
/// datagrams of one flow (GRO), `stride` bytes apart, with only the last one
/// shorter; each segment is classified on its own.
fn split_batch(
    bufs: &mut [IoSliceMut<'_>],
    meta: &mut [RecvMeta],
    count: usize,
    mut deliver: impl FnMut(ReceivedProbe),
) {
    for (buf, meta) in bufs.iter_mut().zip(meta.iter_mut()).take(count) {
        meta.len = keep_quic_segments(buf, meta, &mut deliver);
    }
}

/// Decodes and delivers the buffer's probe segments and packs the QUIC
/// segments that may reach the endpoint to the front, keeping `stride`, so
/// quinn sees only those. Removing a whole segment keeps every later one on a
/// stride boundary. Returns the packed length, zero when nothing is left for
/// quinn.
fn keep_quic_segments(
    buf: &mut [u8],
    meta: &RecvMeta,
    deliver: &mut impl FnMut(ReceivedProbe),
) -> usize {
    let len = meta.len.min(buf.len());
    let stride = if meta.stride == 0 { len } else { meta.stride };
    let mut kept = 0;
    let mut start = 0;
    while start < len {
        let end = start.saturating_add(stride).min(len);
        let segment = &buf[start..end];
        if is_probe_datagram(segment) {
            decode_probe(segment, meta, deliver);
        } else if reaches_endpoint(segment) {
            buf.copy_within(start..end, kept);
            kept += end - start;
        } else {
            tracing::trace!("direct-carrier: dropped a QUIC packet quinn would answer unadmitted");
        }
        start = end;
    }
    kept
}

fn decode_probe(segment: &[u8], meta: &RecvMeta, deliver: &mut impl FnMut(ReceivedProbe)) {
    match ProbeDatagram::decode(segment) {
        Ok(datagram) => deliver(ReceivedProbe {
            datagram,
            source: AddressPolicy::canonical_socket_addr(meta.addr),
            local_ip: meta.dst_ip.map(AddressPolicy::canonical_ip),
        }),
        Err(error) => tracing::trace!(%error, "direct-carrier: dropped a malformed probe datagram"),
    }
}

#[cfg(test)]
mod tests;
