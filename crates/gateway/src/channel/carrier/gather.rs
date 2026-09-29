//! A's host candidates for one answer, and the pairs A punches from them.
//! Address classes come from `AddressPolicy` only; this module ranks and caps.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};

use remote_host_protocol::relay::{
    AddressClass, AddressPolicy, MAX_TCP_CANDIDATES, MAX_UDP_HOST_CANDIDATES,
};

use super::interfaces::InterfaceAddress;

/// At most this many IPv6 global unicast addresses, one per /64.
pub(crate) const MAX_GATEWAY_GUAS: usize = 2;
pub(crate) const MAX_GATEWAY_ULAS: usize = 1;
/// Room for a private and a public IPv4 address.
pub(crate) const MAX_GATEWAY_IPV4_HOSTS: usize = 2;
/// Container bridges and veths, and VPN tunnels: never offered.
pub(crate) const VIRTUAL_INTERFACE_PREFIXES: [&str; 11] = [
    "docker",
    "br-",
    "veth",
    "virbr",
    "cni",
    "lxc",
    "tailscale",
    "wg",
    "tun",
    "utun",
    "zt",
];

const _: () = assert!(
    MAX_GATEWAY_GUAS + MAX_GATEWAY_ULAS + MAX_GATEWAY_IPV4_HOSTS <= MAX_UDP_HOST_CANDIDATES
);

/// A host address's rank, best first: `Lan` (private IPv4 or ULA), then an
/// IPv6 global unicast address, then a public IPv4 address. It orders A's
/// candidates and the pairs A punches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum HostTier {
    Lan,
    Gua,
    PublicIpv4,
}

impl HostTier {
    /// `None` for an excluded address.
    pub(crate) fn of(ip: IpAddr, policy: &AddressPolicy) -> Option<Self> {
        let ip = AddressPolicy::canonical_ip(ip);
        match (policy.classify(ip)?, ip) {
            (AddressClass::Lan, _) => Some(Self::Lan),
            (AddressClass::Public, IpAddr::V6(_)) => Some(Self::Gua),
            (AddressClass::Public, IpAddr::V4(_)) => Some(Self::PublicIpv4),
        }
    }
}

/// Whether `candidate` can be offered or targeted: it has a port and its
/// address has a class.
pub(crate) fn is_candidate(candidate: SocketAddr, policy: &AddressPolicy) -> bool {
    candidate.port() != 0 && HostTier::of(candidate.ip(), policy).is_some()
}

/// A's candidates for one answer.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Gathered {
    pub(crate) udp: Vec<SocketAddr>,
    pub(crate) tcp: Vec<SocketAddr>,
}

/// A's candidates for one answer, from the bound UDP sockets and TCP
/// listeners and the `advertised` TCP candidates (see
/// [`advertised_candidates`]). `enumerate` runs once, and only when a socket
/// or listener is bound, so a gateway with nothing bound never walks its
/// interfaces.
pub(crate) fn gather(
    udp: &[SocketAddr],
    tcp: &[SocketAddr],
    advertised: &[SocketAddr],
    policy: &AddressPolicy,
    enumerate: impl FnOnce() -> Vec<InterfaceAddress>,
) -> Gathered {
    if udp.is_empty() && tcp.is_empty() {
        return Gathered::default();
    }
    let interfaces = enumerate();
    Gathered {
        udp: host_candidates(udp, policy, &interfaces),
        tcp: tcp_candidates(tcp, advertised, policy, &interfaces),
    }
}

/// Why A never offers a configured advertised TCP address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unoffered {
    /// Its address has no class.
    NotACandidate,
    /// [`MAX_TCP_CANDIDATES`] other advertised addresses come before it.
    OverCap,
}

/// The configured advertised TCP addresses A offers, canonical and without
/// repeats, at most [`MAX_TCP_CANDIDATES`] in configuration order, and the
/// index of each one it never offers, with the reason. A repeat of an
/// offered address is neither.
pub(crate) fn advertised_candidates(
    configured: &[SocketAddr],
    policy: &AddressPolicy,
) -> (Vec<SocketAddr>, Vec<(usize, Unoffered)>) {
    let mut offered: Vec<SocketAddr> = Vec::new();
    let mut unoffered = Vec::new();
    for (index, address) in configured.iter().enumerate() {
        let address = AddressPolicy::canonical_socket_addr(*address);
        if !is_candidate(address, policy) {
            unoffered.push((index, Unoffered::NotACandidate));
        } else if !offered.contains(&address) {
            if offered.len() == MAX_TCP_CANDIDATES {
                unoffered.push((index, Unoffered::OverCap));
            } else {
                offered.push(address);
            }
        }
    }
    (offered, unoffered)
}

/// A's TCP candidates while a listener is bound: the `advertised`
/// candidates, then the addresses the bound listeners cover, ranked, at most
/// [`MAX_TCP_CANDIDATES`] in all. The operator named the advertised ones,
/// so the cap never drops them for a host address.
fn tcp_candidates(
    listening: &[SocketAddr],
    advertised: &[SocketAddr],
    policy: &AddressPolicy,
    interfaces: &[InterfaceAddress],
) -> Vec<SocketAddr> {
    if listening.is_empty() {
        return Vec::new();
    }
    let mut candidates: Vec<SocketAddr> = Vec::new();
    for candidate in advertised
        .iter()
        .copied()
        .chain(host_candidates(listening, policy, interfaces))
    {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    candidates.truncate(MAX_TCP_CANDIDATES);
    candidates
}

/// The candidates of the sockets or listeners `bound` to: each usable
/// interface address paired with the port of the bound address of its family
/// that covers it, ranked best first and capped per kind.
fn host_candidates(
    bound: &[SocketAddr],
    policy: &AddressPolicy,
    interfaces: &[InterfaceAddress],
) -> Vec<SocketAddr> {
    if bound.is_empty() {
        return Vec::new();
    }
    let mut ranked: Vec<(HostTier, SocketAddr)> = interfaces
        .iter()
        .filter(|address| {
            address.up_running
                && !address.unstable
                && !VIRTUAL_INTERFACE_PREFIXES
                    .iter()
                    .any(|prefix| address.interface.starts_with(prefix))
        })
        .filter_map(|address| {
            let ip = AddressPolicy::canonical_ip(address.ip);
            let tier = HostTier::of(ip, policy)?;
            let port = bound
                .iter()
                .find(|socket| socket.is_ipv4() == ip.is_ipv4() && covers(socket.ip(), ip))?
                .port();
            Some((tier, SocketAddr::new(ip, port)))
        })
        .collect();
    ranked.sort();
    ranked.dedup();

    let mut gua_prefixes = HashSet::new();
    let (mut guas, mut ulas, mut ipv4) = (0, 0, 0);
    ranked
        .into_iter()
        .filter(|(tier, candidate)| {
            let (count, cap) = match (tier, candidate.ip()) {
                (_, IpAddr::V4(_)) => (&mut ipv4, MAX_GATEWAY_IPV4_HOSTS),
                (HostTier::Gua, ip) => {
                    if !gua_prefixes.insert(AddressPolicy::source_key(ip)) {
                        return false;
                    }
                    (&mut guas, MAX_GATEWAY_GUAS)
                }
                (_, IpAddr::V6(_)) => (&mut ulas, MAX_GATEWAY_ULAS),
            };
            *count += 1;
            *count <= cap
        })
        .map(|(_, candidate)| candidate)
        .collect()
}

/// A socket bound to `bind` receives on `ip`.
fn covers(bind: IpAddr, ip: IpAddr) -> bool {
    bind.is_unspecified() || AddressPolicy::canonical_ip(bind) == ip
}

/// One punch target and the host address A sends it from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PunchPair {
    pub(crate) source: IpAddr,
    pub(crate) target: SocketAddr,
}

/// The pairs A punches for one offer: each of P's host candidates from each
/// of A's host addresses of the same family, the best-ranked target first
/// and, for one target, the best-ranked source first. At most `max_pairs`.
pub(crate) fn punch_pairs(
    own: &[SocketAddr],
    targets: &[SocketAddr],
    policy: &AddressPolicy,
    max_pairs: usize,
) -> Vec<PunchPair> {
    let mut pairs: Vec<((HostTier, HostTier), PunchPair)> = targets
        .iter()
        .filter_map(|target| Some((HostTier::of(target.ip(), policy)?, *target)))
        .flat_map(|(target_tier, target)| {
            own.iter().filter_map(move |source| {
                let source_tier = HostTier::of(source.ip(), policy)?;
                (source.is_ipv4() == target.is_ipv4()).then_some((
                    (target_tier, source_tier),
                    PunchPair {
                        source: source.ip(),
                        target,
                    },
                ))
            })
        })
        .collect();
    pairs.sort_by_key(|(rank, _)| *rank);
    pairs
        .into_iter()
        .map(|(_, pair)| pair)
        .take(max_pairs)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const V4_PORT: u16 = 40_004;
    const V6_PORT: u16 = 40_006;
    const TCP_PORT: u16 = 40_010;

    fn address(interface: &str, ip: &str) -> InterfaceAddress {
        InterfaceAddress {
            interface: interface.to_owned(),
            ip: ip.parse().unwrap(),
            up_running: true,
            unstable: false,
        }
    }

    fn sockets() -> Vec<SocketAddr> {
        vec![
            SocketAddr::new("0.0.0.0".parse().unwrap(), V4_PORT),
            SocketAddr::new("::".parse().unwrap(), V6_PORT),
        ]
    }

    fn socket(ip: &str, port: u16) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), port)
    }

    fn parsed(address: &str) -> SocketAddr {
        address.parse().unwrap()
    }

    /// The test data avoids every range `AddressPolicy::for_tests` treats
    /// differently, so it reads the same under either policy.
    fn policy() -> AddressPolicy {
        AddressPolicy::active()
    }

    #[test]
    fn nothing_bound_never_walks_the_interfaces() {
        let walked = std::cell::Cell::new(false);
        let gathered = gather(&[], &[], &[socket("8.8.8.8", 443)], &policy(), || {
            walked.set(true);
            vec![address("eth0", "192.168.1.2")]
        });
        assert_eq!(gathered, Gathered::default());
        assert!(!walked.get());
    }

    #[test]
    fn one_walk_serves_both_carriers() {
        let walks = std::cell::Cell::new(0);
        let listener = socket("0.0.0.0", TCP_PORT);
        let gathered = gather(&sockets(), &[listener], &[], &policy(), || {
            walks.set(walks.get() + 1);
            vec![address("eth0", "192.168.1.2")]
        });
        assert_eq!(walks.get(), 1);
        assert_eq!(
            gathered,
            Gathered {
                udp: vec![socket("192.168.1.2", V4_PORT)],
                tcp: vec![socket("192.168.1.2", TCP_PORT)],
            }
        );
    }

    #[test]
    fn advertised_addresses_are_offered_canonical_once_and_up_to_the_cap() {
        let configured = [
            socket("8.8.8.8", 443),
            parsed("[::ffff:8.8.4.4]:443"),
            parsed("[fe80::1]:443"),
            socket("8.8.8.8", 443),
            socket("1.1.1.1", 443),
            socket("1.0.0.1", 443),
            socket("9.9.9.9", 443),
            socket("8.8.4.4", 443),
        ];
        let (offered, unoffered) = advertised_candidates(&configured, &policy());
        assert_eq!(
            offered,
            vec![
                socket("8.8.8.8", 443),
                socket("8.8.4.4", 443),
                socket("1.1.1.1", 443),
                socket("1.0.0.1", 443),
            ]
        );
        assert_eq!(offered.len(), MAX_TCP_CANDIDATES);
        assert_eq!(
            unoffered,
            vec![(2, Unoffered::NotACandidate), (6, Unoffered::OverCap)],
            "a repeat of an offered address is neither offered again nor reported"
        );
    }

    #[test]
    fn tcp_candidates_need_a_bound_listener_and_advertised_ones_come_first() {
        let interfaces = vec![
            address("eth0", "192.168.1.2"),
            address("eth0", "10.0.0.2"),
            address("eth0", "2606:4700:1::1"),
            address("eth0", "2606:4700:2::1"),
        ];
        let (advertised, _) = advertised_candidates(
            &[socket("8.8.8.8", 443), socket("::ffff:10.0.0.2", TCP_PORT)],
            &policy(),
        );
        assert!(
            gather(&sockets(), &[], &advertised, &policy(), || interfaces
                .clone())
            .tcp
            .is_empty(),
            "no listener, no TCP candidate"
        );

        let listeners = [socket("0.0.0.0", TCP_PORT), socket("::", TCP_PORT)];
        let tcp = gather(&[], &listeners, &advertised, &policy(), || interfaces).tcp;
        assert_eq!(
            tcp,
            vec![
                socket("8.8.8.8", 443),
                socket("10.0.0.2", TCP_PORT),
                socket("192.168.1.2", TCP_PORT),
                socket("2606:4700:1::1", TCP_PORT),
            ],
            "advertised first, then the covered host addresses by rank, without repeats, capped"
        );
        assert_eq!(tcp.len(), MAX_TCP_CANDIDATES);
    }

    #[test]
    fn a_specific_listener_covers_only_its_own_address() {
        let interfaces = vec![address("eth0", "192.168.1.2"), address("eth1", "10.0.0.2")];
        let listener = socket("192.168.1.2", TCP_PORT);
        let tcp = gather(&[], &[listener], &[], &policy(), || interfaces).tcp;
        assert_eq!(tcp, vec![listener]);
    }

    #[test]
    fn a_candidate_needs_a_port_and_a_class() {
        assert!(is_candidate(socket("192.168.1.2", 1), &policy()));
        assert!(!is_candidate(socket("192.168.1.2", 0), &policy()));
        assert!(!is_candidate(parsed("[fe80::1]:1"), &policy()));
    }

    #[test]
    fn candidates_are_ranked_capped_and_paired_with_their_family_port() {
        let interfaces = vec![
            address("eth0", "8.8.8.8"),
            address("eth0", "2606:4700:1::1"),
            address("eth0", "2606:4700:1::2"),
            address("eth0", "2606:4700:2::1"),
            address("eth0", "2606:4700:3::1"),
            address("eth0", "fd00::1"),
            address("eth0", "fd00::2"),
            address("eth0", "192.168.1.2"),
            address("eth1", "10.0.0.2"),
            address("eth1", "10.0.0.3"),
            address("eth0", "fe80::1"),
            address("eth0", "0.0.0.0"),
        ];
        let candidates = host_candidates(&sockets(), &policy(), &interfaces);
        assert_eq!(
            candidates,
            vec![
                socket("10.0.0.2", V4_PORT),
                socket("10.0.0.3", V4_PORT),
                socket("fd00::1", V6_PORT),
                socket("2606:4700:1::1", V6_PORT),
                socket("2606:4700:2::1", V6_PORT),
            ],
            "two IPv4 by rank, one ULA, and two GUAs from distinct /64s"
        );
    }

    #[test]
    fn down_unstable_and_virtual_interfaces_are_never_offered() {
        let mut down = address("eth0", "192.168.1.2");
        down.up_running = false;
        let mut temporary = address("eth0", "2606:4700:1::9");
        temporary.unstable = true;
        let mut interfaces = vec![down, temporary];
        interfaces.extend(
            VIRTUAL_INTERFACE_PREFIXES
                .iter()
                .map(|prefix| address(&format!("{prefix}0"), "172.17.0.1")),
        );
        interfaces.push(address("eth0", "2606:4700:1::1"));
        let candidates = host_candidates(&sockets(), &policy(), &interfaces);
        assert_eq!(candidates, vec![socket("2606:4700:1::1", V6_PORT)]);
    }

    #[test]
    fn a_specific_bind_covers_only_its_own_address() {
        let bound = [socket("192.168.1.2", V4_PORT)];
        let interfaces = vec![
            address("eth0", "192.168.1.2"),
            address("eth1", "10.0.0.2"),
            address("eth0", "2606:4700:1::1"),
        ];
        let candidates = host_candidates(&bound, &policy(), &interfaces);
        assert_eq!(candidates, vec![socket("192.168.1.2", V4_PORT)]);
    }

    #[test]
    fn pairs_stay_in_family_rank_best_first_and_stop_at_the_cap() {
        let own = [
            socket("192.168.1.2", V4_PORT),
            socket("8.8.8.8", V4_PORT),
            socket("2606:4700:1::1", V6_PORT),
        ];
        let targets = [
            socket("1.1.1.1", 5000),
            socket("2a00:1450::1", 5001),
            socket("10.0.0.9", 5002),
            socket("fe80::9", 5003),
        ];
        let pairs = punch_pairs(&own, &targets, &policy(), usize::MAX);
        let flattened: Vec<(IpAddr, SocketAddr)> = pairs
            .iter()
            .map(|pair| (pair.source, pair.target))
            .collect();
        assert_eq!(
            flattened,
            vec![
                ("192.168.1.2".parse().unwrap(), socket("10.0.0.9", 5002)),
                ("8.8.8.8".parse().unwrap(), socket("10.0.0.9", 5002)),
                (
                    "2606:4700:1::1".parse().unwrap(),
                    socket("2a00:1450::1", 5001)
                ),
                ("192.168.1.2".parse().unwrap(), socket("1.1.1.1", 5000)),
                ("8.8.8.8".parse().unwrap(), socket("1.1.1.1", 5000)),
            ],
            "an excluded target is never punched"
        );
        assert_eq!(punch_pairs(&own, &targets, &policy(), 2), pairs[..2]);
    }
}
