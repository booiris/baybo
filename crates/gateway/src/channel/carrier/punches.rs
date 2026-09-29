//! A's live punches: one per accepted offer, at most
//! `MAX_INFLIGHT_PUNCHES_PER_NODE`, each for `PUNCH_TTL`. A punch's
//! **allowed-IP set** gates QUIC admission. It holds P's sealed host
//! candidate IPs, the `Peer` srflx IP A latched, and the sources of P's
//! authenticated punches. None of it authenticates anyone: Noise IK does.

use std::collections::HashSet;
use std::net::IpAddr;

use carrier::socket::ReceivedProbe;
use device_proto::candidates::{GatewaySealer, OfferId};
use remote_host_protocol::relay::{
    AddressPolicy, MAX_INFLIGHT_PUNCHES_PER_NODE, PUNCH_TTL, PunchId, PunchTag,
};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::DropGuard;

/// Sources one punch may admit through authenticated punches. An observer on
/// the P→A path can race a copy of a punch from its own address, so this
/// bounds what such copies admit.
pub(crate) const MAX_PRFLX_SOURCES_PER_PUNCH: usize = 4;

/// What an accepted offer registers.
pub(crate) struct NewPunch {
    pub(crate) punch_id: PunchId,
    pub(crate) offer_id: OfferId,
    /// P's sealed host candidate IPs that have a class.
    pub(crate) hosts: Vec<IpAddr>,
    /// Where the IPv4 socket's rendezvous replies for this punch go, when A
    /// registers.
    pub(crate) replies: Option<mpsc::Sender<ReceivedProbe>>,
    /// Cancels the punch's tasks when the punch leaves the table.
    pub(crate) tasks: DropGuard,
}

struct LivePunch {
    punch_id: PunchId,
    offer_id: OfferId,
    expires_at: Instant,
    hosts: Vec<IpAddr>,
    srflx: Option<IpAddr>,
    prflx: Vec<IpAddr>,
    /// Punch sequence numbers already accepted.
    seqs: HashSet<u16>,
    replies: Option<mpsc::Sender<ReceivedProbe>>,
    _tasks: DropGuard,
}

impl LivePunch {
    fn allows(&self, ip: IpAddr) -> bool {
        self.hosts.contains(&ip) || self.srflx == Some(ip) || self.prflx.contains(&ip)
    }
}

/// How A judged one received `Punch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PunchAuth {
    /// The tag verified under a live punch, and its source is now in that
    /// punch's allowed set, which admits `sources` authenticated sources.
    Admitted { punch_id: PunchId, sources: usize },
    /// The tag verified, but this sequence number was already used.
    Replayed,
    /// The tag verified, but the punch already admits
    /// [`MAX_PRFLX_SOURCES_PER_PUNCH`] sources.
    Full { punch_id: PunchId },
    /// No live punch's offer produces this tag.
    Rejected,
}

#[derive(Default)]
pub(crate) struct PunchTable {
    /// Oldest first.
    live: Vec<LivePunch>,
}

impl PunchTable {
    /// Registers a punch for [`PUNCH_TTL`] from `now`. At the cap, the oldest
    /// live punch is superseded: its entry and its tasks end, and its id is
    /// returned.
    pub(crate) fn insert(&mut self, punch: NewPunch, now: Instant) -> Option<PunchId> {
        self.prune(now);
        let superseded = (self.live.len() >= MAX_INFLIGHT_PUNCHES_PER_NODE)
            .then(|| self.live.remove(0).punch_id);
        self.live.push(LivePunch {
            punch_id: punch.punch_id,
            offer_id: punch.offer_id,
            expires_at: now + PUNCH_TTL,
            hosts: punch
                .hosts
                .into_iter()
                .map(AddressPolicy::canonical_ip)
                .collect(),
            srflx: None,
            prflx: Vec::new(),
            seqs: HashSet::new(),
            replies: punch.replies,
            _tasks: punch.tasks,
        });
        superseded
    }

    /// Whether `ip` is in the allowed set of a live punch.
    pub(crate) fn admits(&self, ip: IpAddr, now: Instant) -> bool {
        let ip = AddressPolicy::canonical_ip(ip);
        self.live_at(now).any(|punch| punch.allows(ip))
    }

    /// Checks a `Punch` from `source` against every live punch in constant
    /// time per tag. The first punch it verifies under takes it, once per
    /// sequence number, and admits its source.
    pub(crate) fn authenticate(
        &mut self,
        source: IpAddr,
        seq: u16,
        tag: &PunchTag,
        sealer: &GatewaySealer,
        now: Instant,
    ) -> PunchAuth {
        let source = AddressPolicy::canonical_ip(source);
        let Some(punch) = self
            .live
            .iter_mut()
            .filter(|punch| punch.expires_at > now)
            .find(|punch| sealer.verify_punch_tag(&punch.offer_id, seq, tag))
        else {
            return PunchAuth::Rejected;
        };
        if !punch.seqs.insert(seq) {
            return PunchAuth::Replayed;
        }
        if !punch.prflx.contains(&source) {
            if punch.prflx.len() >= MAX_PRFLX_SOURCES_PER_PUNCH {
                return PunchAuth::Full {
                    punch_id: punch.punch_id,
                };
            }
            punch.prflx.push(source);
        }
        PunchAuth::Admitted {
            punch_id: punch.punch_id,
            sources: punch.prflx.len(),
        }
    }

    /// Adds the latched `Peer` srflx IP to the punch's allowed set for the
    /// rest of its life. False when the punch is gone.
    pub(crate) fn allow_srflx(&mut self, punch_id: &PunchId, ip: IpAddr, now: Instant) -> bool {
        match self
            .live
            .iter_mut()
            .find(|punch| punch.punch_id == *punch_id && punch.expires_at > now)
        {
            Some(punch) => {
                punch.srflx = Some(AddressPolicy::canonical_ip(ip));
                true
            }
            None => false,
        }
    }

    /// Where a rendezvous reply for `punch_id` goes, while the punch lives
    /// and A registers for it.
    pub(crate) fn replies_for(
        &self,
        punch_id: &PunchId,
        now: Instant,
    ) -> Option<mpsc::Sender<ReceivedProbe>> {
        self.live_at(now)
            .find(|punch| punch.punch_id == *punch_id)
            .and_then(|punch| punch.replies.clone())
    }

    fn live_at(&self, now: Instant) -> impl Iterator<Item = &LivePunch> {
        self.live.iter().filter(move |punch| punch.expires_at > now)
    }

    fn prune(&mut self, now: Instant) {
        self.live.retain(|punch| punch.expires_at > now);
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use device_proto::candidates::DeviceSealer;
    use device_proto::noise::StaticKeypair;
    use remote_host_protocol::relay::PUNCH_TAG_LEN;
    use tokio_util::sync::CancellationToken;

    use super::*;

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    fn sealers() -> (DeviceSealer, GatewaySealer) {
        let device = StaticKeypair::generate().unwrap();
        let gateway = StaticKeypair::generate().unwrap();
        (
            DeviceSealer::derive(&device.secret(), &gateway.public()).unwrap(),
            GatewaySealer::derive(&gateway.secret(), &device.public()).unwrap(),
        )
    }

    fn punch(hosts: &[&str], tasks: &CancellationToken) -> (NewPunch, PunchId, OfferId) {
        let punch_id = PunchId::generate();
        let offer_id = device_proto::candidates::DeviceOffer::new(0, Vec::new()).offer_id;
        (
            NewPunch {
                punch_id,
                offer_id,
                hosts: hosts.iter().map(|host| ip(host)).collect(),
                replies: None,
                tasks: tasks.clone().drop_guard(),
            },
            punch_id,
            offer_id,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn a_punch_admits_its_hosts_for_the_punch_ttl() {
        let mut table = PunchTable::default();
        let now = Instant::now();
        let (new, _, _) = punch(
            &["192.168.1.7", "::ffff:10.0.0.9"],
            &CancellationToken::new(),
        );
        assert_eq!(table.insert(new, now), None);
        assert!(table.admits(ip("192.168.1.7"), now));
        assert!(table.admits(ip("::ffff:192.168.1.7"), now));
        assert!(table.admits(ip("10.0.0.9"), now));
        assert!(!table.admits(ip("192.168.1.8"), now));
        assert!(!table.admits(ip("192.168.1.7"), now + PUNCH_TTL));
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_punch_at_the_cap_supersedes_the_oldest_and_ends_its_tasks() {
        let mut table = PunchTable::default();
        let now = Instant::now();
        let oldest_tasks = CancellationToken::new();
        let (oldest, oldest_id, _) = punch(&["10.0.0.1"], &oldest_tasks);
        table.insert(oldest, now);
        for _ in 1..MAX_INFLIGHT_PUNCHES_PER_NODE {
            let (next, _, _) = punch(&["10.0.0.2"], &CancellationToken::new());
            table.insert(next, now);
        }
        assert!(!oldest_tasks.is_cancelled());

        let (newest, _, _) = punch(&["10.0.0.3"], &CancellationToken::new());
        assert_eq!(table.insert(newest, now), Some(oldest_id));
        assert!(oldest_tasks.is_cancelled());
        assert!(!table.admits(ip("10.0.0.1"), now));
        assert!(table.admits(ip("10.0.0.3"), now));
    }

    #[tokio::test(start_paused = true)]
    async fn an_authenticated_punch_admits_its_source_once_per_sequence() {
        let (device, gateway) = sealers();
        let mut table = PunchTable::default();
        let now = Instant::now();
        let (new, punch_id, offer_id) = punch(&["10.0.0.1"], &CancellationToken::new());
        table.insert(new, now);
        let source = ip("1.1.1.1");
        assert!(!table.admits(source, now));

        let tag = device.punch_tag(&offer_id, 0).unwrap();
        assert_eq!(
            table.authenticate(source, 0, &tag, &gateway, now),
            PunchAuth::Admitted {
                punch_id,
                sources: 1
            }
        );
        assert!(table.admits(source, now));
        assert_eq!(
            table.authenticate(source, 0, &tag, &gateway, now),
            PunchAuth::Replayed
        );
        let next = device.punch_tag(&offer_id, 1).unwrap();
        assert_eq!(
            table.authenticate(source, 1, &next, &gateway, now),
            PunchAuth::Admitted {
                punch_id,
                sources: 1
            },
            "a source already admitted is not counted again"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_bad_tag_admits_nothing() {
        let (device, gateway) = sealers();
        let mut table = PunchTable::default();
        let now = Instant::now();
        let (new, _, offer_id) = punch(&["10.0.0.1"], &CancellationToken::new());
        table.insert(new, now);
        let source = ip("1.1.1.1");
        let random = PunchTag::from_bytes([0x5a; PUNCH_TAG_LEN]);
        let other_seq = device.punch_tag(&offer_id, 1).unwrap();
        let other_offer = device
            .punch_tag(
                &device_proto::candidates::DeviceOffer::new(0, Vec::new()).offer_id,
                0,
            )
            .unwrap();
        for tag in [random, other_seq, other_offer] {
            assert_eq!(
                table.authenticate(source, 0, &tag, &gateway, now),
                PunchAuth::Rejected
            );
        }
        assert!(!table.admits(source, now));
        let valid = device.punch_tag(&offer_id, 0).unwrap();
        assert_eq!(
            table.authenticate(source, 0, &valid, &gateway, now + PUNCH_TTL),
            PunchAuth::Rejected,
            "an expired punch verifies nothing"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn authenticated_punches_admit_at_most_the_prflx_cap() {
        let (device, gateway) = sealers();
        let mut table = PunchTable::default();
        let now = Instant::now();
        let (new, punch_id, offer_id) = punch(&[], &CancellationToken::new());
        table.insert(new, now);
        for (seq, last) in (0u16..).zip(1..=MAX_PRFLX_SOURCES_PER_PUNCH) {
            let source = IpAddr::V4(Ipv4Addr::new(1, 1, 1, u8::try_from(last).unwrap()));
            let tag = device.punch_tag(&offer_id, seq).unwrap();
            assert!(matches!(
                table.authenticate(source, seq, &tag, &gateway, now),
                PunchAuth::Admitted { sources, .. } if sources == last
            ));
        }
        let seq = u16::try_from(MAX_PRFLX_SOURCES_PER_PUNCH).unwrap();
        let tag = device.punch_tag(&offer_id, seq).unwrap();
        assert_eq!(
            table.authenticate(ip("1.1.1.200"), seq, &tag, &gateway, now),
            PunchAuth::Full { punch_id }
        );
        assert!(!table.admits(ip("1.1.1.200"), now));
    }

    #[tokio::test(start_paused = true)]
    async fn a_latched_srflx_joins_the_allowed_set_of_a_live_punch_only() {
        let mut table = PunchTable::default();
        let now = Instant::now();
        let (new, punch_id, _) = punch(&[], &CancellationToken::new());
        table.insert(new, now);
        assert!(table.allow_srflx(&punch_id, ip("::ffff:1.1.1.1"), now));
        assert!(table.admits(ip("1.1.1.1"), now));
        assert!(!table.allow_srflx(&PunchId::generate(), ip("1.1.1.2"), now));
        assert!(!table.allow_srflx(&punch_id, ip("1.1.1.3"), now + PUNCH_TTL));
    }
}
