//! C's UDP rendezvous: the one socket where the two roles of a punch register,
//! so that C can tell each the IPv4 mapping it observed for the other.
//!
//! It is IPv4 by design, because its job is to observe IPv4 NAT mappings.
//! C answers a `Register` only for a live punch, tagged under the role's key,
//! from the role's latched source, and replies once, to the sender only, with
//! a datagram shorter than the request and tagged under the same key. Anything else is dropped silently, so the
//! socket offers no oracle and cannot reflect or amplify.

use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use remote_host_protocol::relay::{
    AddressPolicy, ProbeDatagram, REGISTER_DATAGRAM_LEN, UdpRendezvous, socket_recv_backoff,
};
use tokio::net::UdpSocket;
use tokio::time::{Instant, MissedTickBehavior};

use crate::control::ControlRegistry;
use crate::error::RendezvousStartError;
use crate::punch::{PUNCH_SWEEP_INTERVAL, PunchRegistry};

/// Where the rendezvous binds when `UDP_BIND_ADDR` is unset.
pub const DEFAULT_UDP_BIND_ADDR: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 7777);
/// At most one warning per reason per interval for datagrams from a source
/// the rendezvous cannot use; the rest log at debug and are counted.
const UDP_SOURCE_WARN_INTERVAL: Duration = Duration::from_secs(60);
/// One byte past the only datagram C accepts, so a longer one reads truncated
/// and fails to decode.
const RECV_BUFFER_LEN: usize = REGISTER_DATAGRAM_LEN + 1;

/// `UDP_PUBLIC_ADDR` as normalised at startup
/// ([`UdpRendezvous::normalize_address`]): what C hands both roles of every
/// punch. A hostname is not resolved at C.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RendezvousAddress(String);

impl RendezvousAddress {
    pub fn parse(value: &str) -> Result<Self, RendezvousStartError> {
        Ok(Self(UdpRendezvous::normalize_address(
            value,
            &AddressPolicy::active(),
        )?))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Parses `UDP_BIND_ADDR`, which must be an IPv4 socket address.
pub fn parse_bind_address(value: &str) -> Result<SocketAddrV4, RendezvousStartError> {
    let address =
        value
            .trim()
            .parse::<SocketAddr>()
            .map_err(|error| RendezvousStartError::BindSyntax {
                reason: error.to_string(),
            })?;
    match address {
        SocketAddr::V4(address) => Ok(address),
        SocketAddr::V6(_) => Err(RendezvousStartError::BindNotIpv4 { address }),
    }
}

/// C's bound rendezvous socket.
pub struct RendezvousServer {
    socket: UdpSocket,
}

impl RendezvousServer {
    pub async fn bind(address: SocketAddrV4) -> Result<Self, RendezvousStartError> {
        let socket =
            UdpSocket::bind(address)
                .await
                .map_err(|error| RendezvousStartError::Bind {
                    address,
                    reason: error.to_string(),
                })?;
        Ok(Self { socket })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Answers `Register`s for `control`'s punches and sweeps expired punches,
    /// until the process exits. A receive error backs off and never ends the
    /// loop, so it cannot stop HTTP or WSS.
    pub async fn serve(self, control: Arc<ControlRegistry>) {
        serve_io(&self.socket, control.punches(), AddressPolicy::active()).await;
    }
}

/// The datagram I/O the rendezvous loop needs, so tests can inject sources
/// and receive errors a real socket cannot produce on demand.
trait RendezvousIo {
    fn recv_from(
        &self,
        buffer: &mut [u8],
    ) -> impl Future<Output = io::Result<(usize, SocketAddr)>> + Send;
    fn send_to(
        &self,
        datagram: &[u8],
        target: SocketAddr,
    ) -> impl Future<Output = io::Result<usize>> + Send;
}

impl RendezvousIo for UdpSocket {
    fn recv_from(
        &self,
        buffer: &mut [u8],
    ) -> impl Future<Output = io::Result<(usize, SocketAddr)>> + Send {
        UdpSocket::recv_from(self, buffer)
    }

    fn send_to(
        &self,
        datagram: &[u8],
        target: SocketAddr,
    ) -> impl Future<Output = io::Result<usize>> + Send {
        UdpSocket::send_to(self, datagram, target)
    }
}

async fn serve_io<I: RendezvousIo>(io: &I, punches: &PunchRegistry, policy: AddressPolicy) {
    let mut buffer = [0u8; RECV_BUFFER_LEN];
    let mut consecutive_errors: u32 = 0;
    let mut warnings = SourceWarnings::default();
    let mut sweep = tokio::time::interval(PUNCH_SWEEP_INTERVAL);
    sweep.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            received = io.recv_from(&mut buffer) => match received {
                Ok((len, source)) => {
                    if consecutive_errors > 0 {
                        tracing::info!(
                            errors = consecutive_errors,
                            "rendezvous: UDP receive recovered"
                        );
                        consecutive_errors = 0;
                    }
                    let Some(reply) =
                        reply_to(&buffer[..len], source, punches, &policy, &mut warnings)
                    else {
                        continue;
                    };
                    if let Err(error) = io.send_to(&reply, source).await {
                        tracing::debug!(%error, "rendezvous: reply send failed");
                    }
                }
                Err(error) => {
                    consecutive_errors = consecutive_errors.saturating_add(1);
                    let wait = socket_recv_backoff(consecutive_errors);
                    if consecutive_errors == 1 {
                        tracing::warn!(
                            %error,
                            ?wait,
                            "rendezvous: UDP receive failed; backing off without stopping HTTP/WSS"
                        );
                    } else {
                        tracing::debug!(
                            %error,
                            errors = consecutive_errors,
                            ?wait,
                            "rendezvous: UDP receive still failing"
                        );
                    }
                    tokio::time::sleep(wait).await;
                }
            },
            _ = sweep.tick() => punches.sweep(),
        }
    }
}

/// C's reply to one received datagram, or `None` to drop it silently. Only a
/// `Register` from a `Public` IPv4 source reaches the punch registry;
/// `::ffff:` sources are canonicalised first.
fn reply_to(
    datagram: &[u8],
    source: SocketAddr,
    punches: &PunchRegistry,
    policy: &AddressPolicy,
    warnings: &mut SourceWarnings,
) -> Option<Vec<u8>> {
    let register = ProbeDatagram::decode(datagram).ok()?;
    let ProbeDatagram::Register { punch_id, role, .. } = register else {
        return None;
    };
    let SocketAddr::V4(observed) = AddressPolicy::canonical_socket_addr(source) else {
        warnings.note(SourceRejection::NotIpv4, source);
        return None;
    };
    if observed.port() == 0 || policy.public_v4(IpAddr::V4(*observed.ip())).is_none() {
        warnings.note(SourceRejection::NotPublic, source);
        return None;
    }
    let reply = punches.register_datagram(&register, observed);
    tracing::debug!(
        punch = %punch_id.tag(),
        ?role,
        source = %observed,
        reply = reply.as_ref().map_or("none", |reply| match reply {
            ProbeDatagram::Registered { .. } => "registered",
            ProbeDatagram::Peer { .. } => "peer",
            ProbeDatagram::Register { .. } | ProbeDatagram::Punch { .. } => "other",
        }),
        "rendezvous: register"
    );
    reply.map(|reply| reply.encode())
}

#[derive(Clone, Copy)]
enum SourceRejection {
    NotIpv4,
    NotPublic,
}

impl SourceRejection {
    fn reason(self) -> &'static str {
        match self {
            Self::NotIpv4 => "udp_source_not_ipv4",
            Self::NotPublic => "udp_source_not_public",
        }
    }
}

#[derive(Default)]
struct WarnWindow {
    last_warn: Option<Instant>,
    suppressed: u64,
}

/// Rate-limits the unusable-source warnings to one per reason per
/// [`UDP_SOURCE_WARN_INTERVAL`], so a source-rewriting deployment stays
/// visible without flooding the log. Addresses appear only at debug.
#[derive(Default)]
struct SourceWarnings {
    not_ipv4: WarnWindow,
    not_public: WarnWindow,
}

impl SourceWarnings {
    fn note(&mut self, rejection: SourceRejection, source: SocketAddr) {
        tracing::debug!(
            reason = rejection.reason(),
            %source,
            "rendezvous: register from an unusable source dropped"
        );
        let window = match rejection {
            SourceRejection::NotIpv4 => &mut self.not_ipv4,
            SourceRejection::NotPublic => &mut self.not_public,
        };
        let now = Instant::now();
        match window.last_warn {
            Some(last) if now.duration_since(last) < UDP_SOURCE_WARN_INTERVAL => {
                window.suppressed = window.suppressed.saturating_add(1);
            }
            _ => {
                tracing::warn!(
                    reason = rejection.reason(),
                    suppressed = window.suppressed,
                    "rendezvous: register from a source C cannot observe as a public IPv4 mapping; is the UDP port published through a source-rewriting proxy?"
                );
                window.last_warn = Some(now);
                window.suppressed = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests;
