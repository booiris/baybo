//! One side's UDP rendezvous for one punch, shared by A and P: register the
//! side's IPv4 socket with C for its role until C returns the other role's
//! mapping, and latch that mapping.

use std::net::{IpAddr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use remote_host_protocol::relay::{
    AddressPolicy, ProbeDatagram, PunchId, PunchRole, RendezvousTicket,
};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until};

use crate::socket::{DemuxSocket, ReceivedProbe};

/// How often a side re-sends its `Register` while it holds no `Peer`. A lost
/// `Register` or a lost `Peer` costs one interval, since C answers every
/// `Register`.
pub const REGISTER_RETRY_INTERVAL: Duration = Duration::from_millis(250);
/// How long a side registers for one punch before it gives up on a `Peer`.
pub const PEER_WAIT: Duration = Duration::from_secs(5);

/// What one received probe datagram meant to a punch's registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerEvent {
    /// C confirmed the ticket; the other role has not registered yet.
    Registered,
    /// The first valid `Peer`: the other role's mapping, now latched.
    Latched(SocketAddrV4),
    /// A valid `Peer` naming the latched mapping again.
    Repeated,
    /// A valid `Peer` naming another mapping than the latched one. It is
    /// dropped and logged as `peer_conflict`.
    Conflict,
    /// Anything else: a datagram from another source than the rendezvous
    /// address, for another punch, or a `Peer` whose mapping is not a
    /// `Public` IPv4 address with a port.
    Ignored,
}

/// The `Peer` rule of one punch. A `Peer` counts only when it comes from the
/// resolved rendezvous address, names this punch, and carries a `Public`
/// IPv4 mapping with a non-zero port; the first such one is latched for the
/// life of the punch.
#[derive(Debug)]
pub struct PeerLatch {
    punch_id: PunchId,
    rendezvous: SocketAddr,
    policy: AddressPolicy,
    latched: Option<SocketAddrV4>,
}

impl PeerLatch {
    pub fn new(punch_id: PunchId, rendezvous: SocketAddrV4, policy: AddressPolicy) -> Self {
        Self {
            punch_id,
            rendezvous: SocketAddr::V4(rendezvous),
            policy,
            latched: None,
        }
    }

    pub fn latched(&self) -> Option<SocketAddrV4> {
        self.latched
    }

    pub fn observe(&mut self, probe: &ReceivedProbe) -> PeerEvent {
        if probe.source != self.rendezvous {
            return PeerEvent::Ignored;
        }
        match probe.datagram {
            ProbeDatagram::Registered { punch_id } if punch_id == self.punch_id => {
                PeerEvent::Registered
            }
            ProbeDatagram::Peer { punch_id, srflx } if punch_id == self.punch_id => {
                if srflx.port() == 0 || self.policy.public_v4(IpAddr::V4(*srflx.ip())).is_none() {
                    return PeerEvent::Ignored;
                }
                match self.latched {
                    None => {
                        self.latched = Some(srflx);
                        PeerEvent::Latched(srflx)
                    }
                    Some(latched) if latched == srflx => PeerEvent::Repeated,
                    Some(latched) => {
                        tracing::info!(punch = %self.punch_id.tag(), "peer_conflict");
                        tracing::debug!(
                            punch = %self.punch_id.tag(),
                            %latched,
                            dropped = %srflx,
                            "peer_conflict: a Peer named another mapping than the latched one"
                        );
                        PeerEvent::Conflict
                    }
                }
            }
            _ => PeerEvent::Ignored,
        }
    }
}

/// How a registration ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterOutcome {
    /// C returned the other role's mapping, now latched.
    Peer(SocketAddrV4),
    /// The deadline passed, or the replies ended, without a valid `Peer`.
    /// `registered` says whether C confirmed the ticket.
    NoPeer { registered: bool },
}

/// One side's registration for one punch: its `Register` datagram, where it
/// goes, and the punch's [`PeerLatch`].
#[derive(Debug)]
pub struct Registration {
    register: ProbeDatagram,
    rendezvous: SocketAddrV4,
    latch: PeerLatch,
    registered: bool,
}

impl Registration {
    /// `rendezvous` is the address `UdpRendezvous::resolve_public_v4`
    /// returned; replies are accepted only from it.
    pub fn new(
        punch_id: PunchId,
        role: PunchRole,
        ticket: RendezvousTicket,
        rendezvous: SocketAddrV4,
        policy: AddressPolicy,
    ) -> Self {
        Self {
            register: ProbeDatagram::Register {
                punch_id,
                role,
                ticket,
            },
            rendezvous,
            latch: PeerLatch::new(punch_id, rendezvous, policy),
            registered: false,
        }
    }

    /// Sends `Register` from `socket` every [`REGISTER_RETRY_INTERVAL`] until
    /// a valid `Peer` arrives on `replies` or `deadline` passes. A
    /// `Registered` reply confirms the ticket and does not end the loop. A
    /// send error is logged and the next interval tries again.
    pub async fn until_peer(
        &mut self,
        socket: &DemuxSocket,
        replies: &mut mpsc::Receiver<ReceivedProbe>,
        deadline: Instant,
    ) -> RegisterOutcome {
        let mut resend = interval(REGISTER_RETRY_INTERVAL);
        resend.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let expired = sleep_until(deadline);
        tokio::pin!(expired);
        loop {
            tokio::select! {
                biased;
                () = &mut expired => return self.no_peer(),
                reply = replies.recv() => {
                    let Some(probe) = reply else {
                        return self.no_peer();
                    };
                    match self.latch.observe(&probe) {
                        PeerEvent::Latched(srflx) => return RegisterOutcome::Peer(srflx),
                        PeerEvent::Registered => self.registered = true,
                        PeerEvent::Repeated | PeerEvent::Conflict | PeerEvent::Ignored => {}
                    }
                }
                _ = resend.tick() => {
                    if let Err(error) = socket
                        .send_probe(&self.register, SocketAddr::V4(self.rendezvous), None)
                        .await
                    {
                        tracing::debug!(%error, "direct-carrier: Register send failed");
                    }
                }
            }
        }
    }

    /// Keeps judging replies until `deadline`, after the `Peer` is latched,
    /// so a `Peer` that names another mapping is logged and dropped.
    pub async fn watch(&mut self, replies: &mut mpsc::Receiver<ReceivedProbe>, deadline: Instant) {
        let expired = sleep_until(deadline);
        tokio::pin!(expired);
        loop {
            tokio::select! {
                biased;
                () = &mut expired => return,
                reply = replies.recv() => match reply {
                    Some(probe) => {
                        self.latch.observe(&probe);
                    }
                    None => return,
                },
            }
        }
    }

    fn no_peer(&self) -> RegisterOutcome {
        RegisterOutcome::NoPeer {
            registered: self.registered,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use remote_host_protocol::relay::RENDEZVOUS_TICKET_LEN;
    use tokio::net::UdpSocket;
    use tokio::time::timeout;

    use super::*;

    const RENDEZVOUS: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, 1), 7777);
    const SRFLX: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(8, 8, 4, 4), 40_000);
    const OTHER_SRFLX: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(8, 8, 4, 4), 40_001);
    const TEST_TIMEOUT: Duration = Duration::from_secs(10);
    const REPLY_BUFFER_LEN: usize = 128;

    fn from(source: SocketAddrV4, datagram: ProbeDatagram) -> ReceivedProbe {
        ReceivedProbe {
            datagram,
            source: SocketAddr::V4(source),
            local_ip: None,
        }
    }

    fn peer(punch_id: PunchId, srflx: SocketAddrV4) -> ProbeDatagram {
        ProbeDatagram::Peer { punch_id, srflx }
    }

    fn latch(punch_id: PunchId) -> PeerLatch {
        PeerLatch::new(punch_id, RENDEZVOUS, AddressPolicy::active())
    }

    #[test]
    fn a_peer_counts_only_from_the_rendezvous_for_this_punch_and_a_public_mapping() {
        let punch_id = PunchId::generate();
        let mut latch = latch(punch_id);
        let elsewhere = SocketAddrV4::new(*RENDEZVOUS.ip(), RENDEZVOUS.port() + 1);
        let private = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 7), 40_000);
        let no_port = SocketAddrV4::new(*SRFLX.ip(), 0);
        for ignored in [
            from(elsewhere, peer(punch_id, SRFLX)),
            from(RENDEZVOUS, peer(PunchId::generate(), SRFLX)),
            from(RENDEZVOUS, peer(punch_id, private)),
            from(RENDEZVOUS, peer(punch_id, no_port)),
            from(
                RENDEZVOUS,
                ProbeDatagram::Registered {
                    punch_id: PunchId::generate(),
                },
            ),
        ] {
            assert_eq!(latch.observe(&ignored), PeerEvent::Ignored);
        }
        assert_eq!(latch.latched(), None);
        assert_eq!(
            latch.observe(&from(RENDEZVOUS, ProbeDatagram::Registered { punch_id })),
            PeerEvent::Registered
        );
    }

    #[test]
    fn the_first_valid_peer_is_latched_and_a_conflicting_one_is_dropped() {
        let punch_id = PunchId::generate();
        let mut latch = latch(punch_id);
        assert_eq!(
            latch.observe(&from(RENDEZVOUS, peer(punch_id, SRFLX))),
            PeerEvent::Latched(SRFLX)
        );
        assert_eq!(
            latch.observe(&from(RENDEZVOUS, peer(punch_id, SRFLX))),
            PeerEvent::Repeated
        );
        assert_eq!(
            latch.observe(&from(RENDEZVOUS, peer(punch_id, OTHER_SRFLX))),
            PeerEvent::Conflict
        );
        assert_eq!(latch.latched(), Some(SRFLX));
    }

    /// A stand-in for C's rendezvous socket on loopback.
    async fn mock_c() -> (UdpSocket, SocketAddrV4) {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let SocketAddr::V4(address) = socket.local_addr().unwrap() else {
            panic!("an IPv4 bind has an IPv4 address");
        };
        (socket, address)
    }

    async fn next_register(c: &UdpSocket) -> (ProbeDatagram, SocketAddr) {
        let mut buffer = [0u8; REPLY_BUFFER_LEN];
        let (len, source) = timeout(TEST_TIMEOUT, c.recv_from(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        (ProbeDatagram::decode(&buffer[..len]).unwrap(), source)
    }

    /// A side's socket with a client endpoint, so its driver reads and probe
    /// datagrams reach the queue.
    fn side_socket() -> (
        std::sync::Arc<DemuxSocket>,
        quinn::Endpoint,
        mpsc::Receiver<ReceivedProbe>,
    ) {
        let (socket, probes) = DemuxSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let endpoint = socket.quic_endpoint(None).unwrap();
        (socket, endpoint, probes)
    }

    #[tokio::test]
    async fn registration_repeats_after_registered_and_recovers_a_lost_peer() {
        let (c, rendezvous) = mock_c().await;
        let (socket, _endpoint, mut replies) = side_socket();
        let punch_id = PunchId::generate();
        let ticket = RendezvousTicket::from_bytes([3; RENDEZVOUS_TICKET_LEN]);
        let mut registration = Registration::new(
            punch_id,
            PunchRole::Gateway,
            ticket.clone(),
            rendezvous,
            AddressPolicy::active(),
        );
        let c_side = async {
            let (first, side) = next_register(&c).await;
            assert_eq!(
                first,
                ProbeDatagram::Register {
                    punch_id,
                    role: PunchRole::Gateway,
                    ticket,
                }
            );
            let registered = ProbeDatagram::Registered { punch_id }.encode();
            c.send_to(&registered, side).await.unwrap();
            // The second Register's Peer is lost; the third's arrives.
            next_register(&c).await;
            next_register(&c).await;
            c.send_to(&peer(punch_id, SRFLX).encode(), side)
                .await
                .unwrap();
        };
        let deadline = Instant::now() + TEST_TIMEOUT;
        let (outcome, ()) = tokio::join!(
            registration.until_peer(&socket, &mut replies, deadline),
            c_side
        );
        assert_eq!(outcome, RegisterOutcome::Peer(SRFLX));
    }

    #[tokio::test]
    async fn registration_without_a_peer_ends_at_the_deadline() {
        let (c, rendezvous) = mock_c().await;
        let (socket, _endpoint, mut replies) = side_socket();
        let punch_id = PunchId::generate();
        let mut registration = Registration::new(
            punch_id,
            PunchRole::Device,
            RendezvousTicket::from_bytes([4; RENDEZVOUS_TICKET_LEN]),
            rendezvous,
            AddressPolicy::active(),
        );
        let deadline = Instant::now() + REGISTER_RETRY_INTERVAL * 2;
        let outcome = registration
            .until_peer(&socket, &mut replies, deadline)
            .await;
        assert_eq!(outcome, RegisterOutcome::NoPeer { registered: false });
        let (register, _) = next_register(&c).await;
        assert!(matches!(
            register,
            ProbeDatagram::Register {
                role: PunchRole::Device,
                ..
            }
        ));
    }
}
