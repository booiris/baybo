//! One side's UDP rendezvous for one punch, shared by A and P: register the
//! side's IPv4 socket with C for its role until C returns the other role's
//! mapping, and latch that mapping. Every exchange is tagged under the role's
//! key, which C delivered over TLS.

use std::net::{IpAddr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use remote_host_protocol::relay::{
    AddressPolicy, ProbeDatagram, PunchId, PunchRole, RendezvousKey,
};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until};

use crate::socket::{DemuxSocket, ReceivedProbe};

/// How often a side re-sends its `Register` while it holds no `Peer`. A lost
/// `Register` or a lost `Peer` costs one interval, since C answers every
/// `Register`.
pub const REGISTER_RETRY_INTERVAL: Duration = Duration::from_millis(250);

/// The addresses on a side's own interfaces. Neither the rendezvous address
/// nor a `Peer` mapping may be one of them: a datagram to its own address is
/// delivered locally, past the side's firewall, to whatever listens there.
/// Loopback addresses are left out: the address policy already refuses them
/// in production, and the test policy reaches its fakes through them.
#[derive(Debug, Clone, Default)]
pub struct OwnAddresses(Vec<IpAddr>);

impl OwnAddresses {
    pub fn new(addresses: impl IntoIterator<Item = IpAddr>) -> Self {
        Self(
            addresses
                .into_iter()
                .map(AddressPolicy::canonical_ip)
                .filter(|ip| !ip.is_loopback())
                .collect(),
        )
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = AddressPolicy::canonical_ip(ip);
        self.0.contains(&ip)
    }
}

/// What one received probe datagram meant to a punch's registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerEvent {
    /// C verified the `Register`; the other role has not registered yet.
    Registered,
    /// The first valid `Peer`: the other role's mapping, now latched.
    Latched(SocketAddrV4),
    /// A valid `Peer` naming the latched mapping again.
    Repeated,
    /// A valid `Peer` naming another mapping than the latched one. It is
    /// dropped and logged as `peer_conflict`.
    Conflict,
    /// Anything else: a datagram from another source than the rendezvous
    /// address, for another punch, not tagged under the role's key, or a
    /// `Peer` whose mapping is not a `Public` IPv4 address with a port or is
    /// one of the side's own addresses.
    Ignored,
}

/// The `Peer` rule of one punch. A reply counts only when it comes from the
/// resolved rendezvous address, names this punch and is tagged under the
/// role's key; a `Peer` must also carry a `Public` IPv4 mapping with a
/// non-zero port that is none of the side's own addresses. The first such
/// `Peer` is latched for the life of the punch.
#[derive(Debug)]
pub struct PeerLatch {
    punch_id: PunchId,
    rendezvous: SocketAddr,
    key: RendezvousKey,
    policy: AddressPolicy,
    own: OwnAddresses,
    latched: Option<SocketAddrV4>,
}

impl PeerLatch {
    pub fn new(
        punch_id: PunchId,
        rendezvous: SocketAddrV4,
        key: RendezvousKey,
        policy: AddressPolicy,
        own: OwnAddresses,
    ) -> Self {
        Self {
            punch_id,
            rendezvous: SocketAddr::V4(rendezvous),
            key,
            policy,
            own,
            latched: None,
        }
    }

    pub fn latched(&self) -> Option<SocketAddrV4> {
        self.latched
    }

    pub fn observe(&mut self, probe: &ReceivedProbe) -> PeerEvent {
        if probe.source != self.rendezvous || !self.key.verifies(&probe.datagram) {
            return PeerEvent::Ignored;
        }
        match probe.datagram {
            ProbeDatagram::Registered { punch_id, .. } if punch_id == self.punch_id => {
                PeerEvent::Registered
            }
            ProbeDatagram::Peer {
                punch_id, srflx, ..
            } if punch_id == self.punch_id => {
                let ip = IpAddr::V4(*srflx.ip());
                if srflx.port() == 0 || self.policy.public_v4(ip).is_none() || self.own.contains(ip)
                {
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
    /// `registered` says whether C verified the `Register`.
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
    /// returned; replies are accepted only from it. `None` when it is one of
    /// the side's `own` addresses, or when the key yields no tag: the side
    /// then does not register.
    pub fn new(
        punch_id: PunchId,
        role: PunchRole,
        key: RendezvousKey,
        rendezvous: SocketAddrV4,
        policy: AddressPolicy,
        own: OwnAddresses,
    ) -> Option<Self> {
        if own.contains(IpAddr::V4(*rendezvous.ip())) {
            return None;
        }
        Some(Self {
            register: key.register(punch_id, role)?,
            rendezvous,
            latch: PeerLatch::new(punch_id, rendezvous, key, policy, own),
            registered: false,
        })
    }

    /// Sends `Register` from `socket` every [`REGISTER_RETRY_INTERVAL`] until
    /// a valid `Peer` arrives on `replies` or `deadline` passes. A
    /// `Registered` reply confirms the `Register` and does not end the loop. A
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

    use remote_host_protocol::relay::RENDEZVOUS_KEY_LEN;
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

    const OWN_PUBLIC: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(9, 9, 9, 9), 40_000);

    fn key() -> RendezvousKey {
        RendezvousKey::from_bytes([3; RENDEZVOUS_KEY_LEN])
    }

    fn peer(punch_id: PunchId, srflx: SocketAddrV4) -> ProbeDatagram {
        key().peer(punch_id, srflx).unwrap()
    }

    fn registered(punch_id: PunchId) -> ProbeDatagram {
        key().registered(punch_id).unwrap()
    }

    fn own() -> OwnAddresses {
        OwnAddresses::new([IpAddr::V4(*OWN_PUBLIC.ip())])
    }

    fn latch(punch_id: PunchId) -> PeerLatch {
        PeerLatch::new(punch_id, RENDEZVOUS, key(), AddressPolicy::active(), own())
    }

    #[test]
    fn a_rendezvous_at_an_own_address_is_never_registered_with() {
        let at_own = SocketAddrV4::new(*OWN_PUBLIC.ip(), 7777);
        assert!(
            Registration::new(
                PunchId::generate(),
                PunchRole::Gateway,
                key(),
                at_own,
                AddressPolicy::active(),
                own()
            )
            .is_none()
        );
        assert!(
            Registration::new(
                PunchId::generate(),
                PunchRole::Gateway,
                key(),
                RENDEZVOUS,
                AddressPolicy::active(),
                own()
            )
            .is_some()
        );
        assert!(
            OwnAddresses::new(["::ffff:9.9.9.9".parse().unwrap()])
                .contains(IpAddr::V4(*OWN_PUBLIC.ip())),
            "an IPv4-mapped own address is the same address"
        );
        assert!(
            !OwnAddresses::new([IpAddr::V4(Ipv4Addr::LOCALHOST)])
                .contains(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            "loopback is left to the address policy"
        );
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
            from(RENDEZVOUS, peer(punch_id, OWN_PUBLIC)),
            from(
                RENDEZVOUS,
                RendezvousKey::generate().peer(punch_id, SRFLX).unwrap(),
            ),
            from(RENDEZVOUS, registered(PunchId::generate())),
            from(
                RENDEZVOUS,
                RendezvousKey::generate().registered(punch_id).unwrap(),
            ),
        ] {
            assert_eq!(latch.observe(&ignored), PeerEvent::Ignored);
        }
        assert_eq!(latch.latched(), None);
        assert_eq!(
            latch.observe(&from(RENDEZVOUS, registered(punch_id))),
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
        let mut registration = Registration::new(
            punch_id,
            PunchRole::Gateway,
            key(),
            rendezvous,
            AddressPolicy::active(),
            own(),
        )
        .unwrap();
        let c_side = async {
            let (first, side) = next_register(&c).await;
            assert_eq!(first, key().register(punch_id, PunchRole::Gateway).unwrap());
            let forged = RendezvousKey::generate()
                .peer(punch_id, OTHER_SRFLX)
                .unwrap();
            c.send_to(&forged.encode(), side).await.unwrap();
            c.send_to(&registered(punch_id).encode(), side)
                .await
                .unwrap();
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
            key(),
            rendezvous,
            AddressPolicy::active(),
            own(),
        )
        .unwrap();
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
