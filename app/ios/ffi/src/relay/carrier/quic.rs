//! P's end of a direct carrier: the QUIC client connection a probe won, and
//! the legs dialed on it. Every leg opens one bidirectional stream, writes
//! `DirectOpen{token, class}`, and then runs the same Noise IK handshake as a
//! relay leg, which P confirms.

use std::future::Future;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use carrier::framing::{FrameReader, write_direct_open};
use carrier::socket::DemuxSocket;
use remote_host_protocol::relay::{DirectOpen, DirectToken, LegClass};

use crate::transport::{CarrierStream, LegSocket};

/// Bounds a carrier leg dial, `DirectOpen` through the handshake: a
/// blackholed carrier costs one slow dial before the relay takes the leg.
pub(crate) const DIRECT_LEG_DIAL_TIMEOUT: Duration = Duration::from_secs(3);

/// The application error code P closes a carrier with.
const CARRIER_RETIRED: u32 = 0;

/// A live carrier: the connection, the `DirectOpen` gate A sealed into its
/// answer, and the socket and endpoint the connection rides, which close with
/// it.
#[derive(Clone)]
pub(crate) struct CarrierHandle {
    connection: quinn::Connection,
    token: DirectToken,
    _endpoint: quinn::Endpoint,
    _socket: Arc<DemuxSocket>,
}

impl CarrierHandle {
    pub(crate) fn new(
        connection: quinn::Connection,
        token: DirectToken,
        endpoint: quinn::Endpoint,
        socket: Arc<DemuxSocket>,
    ) -> Self {
        Self {
            connection,
            token,
            _endpoint: endpoint,
            _socket: socket,
        }
    }

    /// The local address the carrier's datagrams leave from, when the socket
    /// reports it.
    pub(crate) fn local_ip(&self) -> Option<IpAddr> {
        self.connection.local_ip()
    }

    /// Closes this one connection; the endpoint stays for whatever else
    /// shares its socket.
    pub(crate) fn close(&self, reason: &str) {
        self.connection
            .close(quinn::VarInt::from_u32(CARRIER_RETIRED), reason.as_bytes());
    }

    /// Resolves when the connection is closed, by either side.
    pub(crate) async fn closed(&self) -> quinn::ConnectionError {
        self.connection.closed().await
    }

    /// Opens one stream and writes its `DirectOpen` preface. Once this
    /// returns, A has the stream: for a chat rotation, it is the commit point.
    pub(crate) async fn open(&self, class: LegClass) -> Result<LegSocket, String> {
        let (mut send, recv) = self
            .connection
            .open_bi()
            .await
            .map_err(|e| format!("open carrier stream: {e}"))?;
        write_direct_open(
            &mut send,
            &DirectOpen {
                token: self.token.clone(),
                class,
            },
        )
        .await
        .map_err(|e| format!("write DirectOpen: {e}"))?;
        Ok(LegSocket::Carrier(CarrierStream::new(
            send,
            FrameReader::new(recv),
        )))
    }

    /// Whether a dial that failed condemns the connection: it is closed, or
    /// it received no datagram while the dial ran (`rx_before` is
    /// [`Self::received`] from before the dial). Anything else belongs to the
    /// stream — A's caps, a slow device lookup — and leaves the carrier up.
    pub(crate) fn connection_failed(&self, rx_before: u64) -> bool {
        self.connection.close_reason().is_some() || self.received() == rx_before
    }

    /// Datagrams received on the connection so far.
    pub(crate) fn received(&self) -> u64 {
        self.connection.stats().udp_rx.datagrams
    }
}

/// How a carrier leg dial failed.
#[derive(Debug)]
pub(crate) enum DialFailure {
    /// The connection is closed or went silent: retire the carrier.
    Connection(String),
    /// The stream failed on a connection that is still talking.
    Stream(String),
    /// The caller withdrew before the stream was opened; nothing touched
    /// the carrier.
    Withdrawn,
}

impl DialFailure {
    pub(crate) fn reason(&self) -> &str {
        match self {
            Self::Connection(reason) | Self::Stream(reason) => reason,
            Self::Withdrawn => "withdrawn before the stream opened",
        }
    }
}

/// Dials one leg of `class` on `carrier`: asks `proceed` once more, opens
/// the stream, then runs `handshake` over it, all within
/// [`DIRECT_LEG_DIAL_TIMEOUT`]. `proceed` is a chat rotation's commit point;
/// a `false` withdraws the dial before anything reaches the gateway. A
/// failure is judged by [`CarrierHandle::connection_failed`].
pub(crate) async fn dial_leg<T, P, F, Fut>(
    carrier: &CarrierHandle,
    class: LegClass,
    proceed: P,
    handshake: F,
) -> Result<T, DialFailure>
where
    P: FnOnce() -> bool,
    F: FnOnce(LegSocket) -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    if !proceed() {
        return Err(DialFailure::Withdrawn);
    }
    let rx_before = carrier.received();
    let dial = async {
        let socket = carrier.open(class).await?;
        handshake(socket).await
    };
    let reason = match tokio::time::timeout(DIRECT_LEG_DIAL_TIMEOUT, dial).await {
        Ok(Ok(leg)) => return Ok(leg),
        Ok(Err(reason)) => reason,
        Err(_) => format!(
            "carrier leg dial exceeded {}s",
            DIRECT_LEG_DIAL_TIMEOUT.as_secs()
        ),
    };
    if carrier.connection_failed(rx_before) {
        Err(DialFailure::Connection(reason))
    } else {
        Err(DialFailure::Stream(reason))
    }
}
