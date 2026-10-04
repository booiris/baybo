//! The one home of address classification for direct carriers. A, C and P all
//! classify through [`AddressPolicy`]; nothing else re-derives an address class.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// The class of an address a direct carrier may use. An address with no class
/// ([`AddressPolicy::classify`] returns `None`) is excluded: nobody offers,
/// targets or admits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AddressClass {
    /// Private IPv4 (including CGNAT and IPv4 link-local) and IPv6 ULA.
    Lan,
    /// Globally routable unicast.
    Public,
}

/// Classifies addresses for direct carriers. Obtain it from
/// [`AddressPolicy::active`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressPolicy {
    test_ranges: bool,
}

/// The key of every per-source cap: an IPv4 address, or an IPv6 /64, so a
/// client rotating through its /64 counts as one source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceKey(IpAddr);

type V4Range = (Ipv4Addr, u32);
type V6Range = (Ipv6Addr, u32);

const IPV4_LAN: [V4Range; 5] = [
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
];

const IPV4_EXCLUDED: [V4Range; 10] = [
    (Ipv4Addr::new(0, 0, 0, 0), 8),
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(192, 0, 0, 0), 24),
    (Ipv4Addr::new(192, 88, 99, 0), 24),
    (Ipv4Addr::new(198, 18, 0, 0), 15),
    (Ipv4Addr::new(192, 0, 2, 0), 24),
    (Ipv4Addr::new(198, 51, 100, 0), 24),
    (Ipv4Addr::new(203, 0, 113, 0), 24),
    (Ipv4Addr::new(224, 0, 0, 0), 4),
    (Ipv4Addr::new(240, 0, 0, 0), 4),
];

const IPV4_TEST_PUBLIC: [V4Range; 2] = [
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(198, 18, 0, 0), 15),
];

const IPV6_LAN: V6Range = (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7);

const IPV6_GLOBAL_UNICAST: V6Range = (Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0), 3);

const IPV6_EXCLUDED_GLOBAL: [V6Range; 4] = [
    (Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 0), 32),
    (Ipv6Addr::new(0x2001, 0x0002, 0, 0, 0, 0, 0, 0), 48),
    (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 32),
    (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
];

const IPV6_TEST_PUBLIC: V6Range = (Ipv6Addr::new(0x2001, 0x0002, 0, 0, 0, 0, 0, 0), 48);

const SOURCE_KEY_IPV6_PREFIX_LEN: u32 = 64;
/// The IPv6 prefix one client is assumed to hold: an end site is routinely
/// delegated a /48, so C's per-client budgets count a /48 as one client.
const CLIENT_KEY_IPV6_PREFIX_LEN: u32 = 48;

impl AddressPolicy {
    /// The policy every caller uses: [`AddressPolicy::for_tests`] when the
    /// crate is built with `test-support`, the production table otherwise.
    pub fn active() -> Self {
        Self {
            test_ranges: cfg!(feature = "test-support"),
        }
    }

    #[cfg(test)]
    pub(crate) const fn production() -> Self {
        Self { test_ranges: false }
    }

    /// The production table, except that 127/8, 198.18/15 and 2001:2::/48
    /// are `Public` and `::1` is `Lan`, so in-process and netns tests can run
    /// over loopback and the benchmarking ranges.
    #[cfg(any(test, feature = "test-support"))]
    pub const fn for_tests() -> Self {
        Self { test_ranges: true }
    }

    /// The address's class, after IPv4-mapped IPv6 is canonicalised to IPv4.
    /// `None` means excluded.
    pub fn classify(&self, ip: IpAddr) -> Option<AddressClass> {
        match Self::canonical_ip(ip) {
            IpAddr::V4(ip) => self.classify_v4(ip),
            IpAddr::V6(ip) => self.classify_v6(ip),
        }
    }

    /// The canonical IPv4 address when `ip` is a `Public` IPv4 address.
    pub fn public_v4(&self, ip: IpAddr) -> Option<Ipv4Addr> {
        match Self::canonical_ip(ip) {
            IpAddr::V4(ip) if self.classify_v4(ip) == Some(AddressClass::Public) => Some(ip),
            _ => None,
        }
    }

    /// `::ffff:a.b.c.d` becomes `a.b.c.d`; every other address is unchanged.
    pub fn canonical_ip(ip: IpAddr) -> IpAddr {
        match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            IpAddr::V4(_) => ip,
        }
    }

    /// [`AddressPolicy::canonical_ip`] applied to a socket address.
    pub fn canonical_socket_addr(address: SocketAddr) -> SocketAddr {
        SocketAddr::new(Self::canonical_ip(address.ip()), address.port())
    }

    /// The per-source cap key of `ip`: the canonical IPv4 address, or the
    /// IPv6 /64.
    pub fn source_key(ip: IpAddr) -> SourceKey {
        Self::prefix_key(ip, SOURCE_KEY_IPV6_PREFIX_LEN)
    }

    /// The key of C's per-client budgets: the canonical IPv4 address, or the
    /// IPv6 /48, so a client cannot multiply its share by rotating through
    /// the /64s of its own delegation.
    pub fn client_key(ip: IpAddr) -> SourceKey {
        Self::prefix_key(ip, CLIENT_KEY_IPV6_PREFIX_LEN)
    }

    fn prefix_key(ip: IpAddr, ipv6_prefix_len: u32) -> SourceKey {
        match Self::canonical_ip(ip) {
            IpAddr::V4(v4) => SourceKey(IpAddr::V4(v4)),
            IpAddr::V6(v6) => SourceKey(IpAddr::V6(Ipv6Addr::from(
                u128::from(v6) & v6_mask(ipv6_prefix_len),
            ))),
        }
    }

    fn classify_v4(&self, ip: Ipv4Addr) -> Option<AddressClass> {
        if self.test_ranges && IPV4_TEST_PUBLIC.iter().any(|range| in_v4(ip, *range)) {
            return Some(AddressClass::Public);
        }
        if IPV4_LAN.iter().any(|range| in_v4(ip, *range)) {
            return Some(AddressClass::Lan);
        }
        if IPV4_EXCLUDED.iter().any(|range| in_v4(ip, *range)) {
            return None;
        }
        Some(AddressClass::Public)
    }

    fn classify_v6(&self, ip: Ipv6Addr) -> Option<AddressClass> {
        if self.test_ranges {
            if ip == Ipv6Addr::LOCALHOST {
                return Some(AddressClass::Lan);
            }
            if in_v6(ip, IPV6_TEST_PUBLIC) {
                return Some(AddressClass::Public);
            }
        }
        if in_v6(ip, IPV6_LAN) {
            return Some(AddressClass::Lan);
        }
        if in_v6(ip, IPV6_GLOBAL_UNICAST)
            && !IPV6_EXCLUDED_GLOBAL.iter().any(|range| in_v6(ip, *range))
        {
            return Some(AddressClass::Public);
        }
        None
    }
}

fn in_v4(ip: Ipv4Addr, (network, prefix_len): V4Range) -> bool {
    let mask = u32::MAX.checked_shl(u32::BITS - prefix_len).unwrap_or(0);
    u32::from(ip) & mask == u32::from(network)
}

fn in_v6(ip: Ipv6Addr, (network, prefix_len): V6Range) -> bool {
    u128::from(ip) & v6_mask(prefix_len) == u128::from(network)
}

fn v6_mask(prefix_len: u32) -> u128 {
    u128::MAX.checked_shl(u128::BITS - prefix_len).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    #[test]
    fn production_class_table() {
        let policy = AddressPolicy::production();
        for lan in [
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.2",
            "100.64.0.1",
            "100.127.255.254",
            "169.254.1.2",
            "fc00::1",
            "fd12:3456::1",
        ] {
            assert_eq!(policy.classify(ip(lan)), Some(AddressClass::Lan), "{lan}");
        }
        for public in [
            "8.8.8.8",
            "1.1.1.1",
            "172.32.0.1",
            "100.128.0.1",
            "192.0.1.1",
            "198.17.255.255",
            "198.20.0.1",
            "223.255.255.254",
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
            "2a00:1450::1",
            "2001:3::1",
            "2c0f:f248::1",
        ] {
            assert_eq!(
                policy.classify(ip(public)),
                Some(AddressClass::Public),
                "{public}"
            );
        }
        for excluded in [
            "0.0.0.0",
            "0.1.2.3",
            "127.0.0.1",
            "127.255.255.254",
            "192.0.0.1",
            "192.88.99.1",
            "198.18.0.1",
            "198.19.255.254",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "239.255.255.255",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "2001:2::1",
            "2001::1",
            "2001:0:4136:e378::1",
            "2002:c000:204::1",
            "64:ff9b::808:808",
            "fec0::1",
            "4000::1",
        ] {
            assert_eq!(policy.classify(ip(excluded)), None, "{excluded}");
        }
    }

    #[test]
    fn test_policy_opens_loopback_and_benchmark_ranges_only() {
        let policy = AddressPolicy::for_tests();
        for public in ["127.0.0.1", "198.18.0.10", "2001:2::10", "2001:2:0:a::2"] {
            assert_eq!(
                policy.classify(ip(public)),
                Some(AddressClass::Public),
                "{public}"
            );
        }
        assert_eq!(policy.classify(ip("::1")), Some(AddressClass::Lan));
        assert_eq!(policy.classify(ip("10.0.1.2")), Some(AddressClass::Lan));
        for excluded in ["0.0.0.0", "192.0.2.1", "fe80::1", "2001:db8::1", "::"] {
            assert_eq!(policy.classify(ip(excluded)), None, "{excluded}");
        }
    }

    #[test]
    fn active_policy_follows_the_test_support_feature() {
        let expected = if cfg!(feature = "test-support") {
            AddressPolicy::for_tests()
        } else {
            AddressPolicy::production()
        };
        assert_eq!(AddressPolicy::active(), expected);
    }

    #[test]
    fn ipv4_mapped_addresses_are_classified_as_ipv4() {
        let policy = AddressPolicy::production();
        assert_eq!(
            policy.classify(ip("::ffff:8.8.8.8")),
            Some(AddressClass::Public)
        );
        assert_eq!(
            policy.classify(ip("::ffff:192.168.1.1")),
            Some(AddressClass::Lan)
        );
        assert_eq!(policy.classify(ip("::ffff:127.0.0.1")), None);
        let mapped: SocketAddr = "[::ffff:8.8.8.8]:7777".parse().unwrap();
        assert_eq!(
            AddressPolicy::canonical_socket_addr(mapped),
            "8.8.8.8:7777".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            policy.public_v4(mapped.ip()),
            Some(Ipv4Addr::new(8, 8, 8, 8))
        );
    }

    #[test]
    fn public_v4_rejects_lan_excluded_and_ipv6() {
        let policy = AddressPolicy::production();
        assert_eq!(
            policy.public_v4(ip("1.2.3.4")),
            Some(Ipv4Addr::new(1, 2, 3, 4))
        );
        for rejected in ["10.0.0.1", "127.0.0.1", "2606:4700:4700::1111"] {
            assert_eq!(policy.public_v4(ip(rejected)), None, "{rejected}");
        }
    }

    #[test]
    fn source_key_is_the_ipv4_address_or_the_ipv6_slash_64() {
        assert_eq!(
            AddressPolicy::source_key(ip("203.0.113.9")),
            AddressPolicy::source_key(ip("::ffff:203.0.113.9"))
        );
        assert_ne!(
            AddressPolicy::source_key(ip("203.0.113.9")),
            AddressPolicy::source_key(ip("203.0.113.10"))
        );
        assert_eq!(
            AddressPolicy::source_key(ip("2001:db8:1:2::1")),
            AddressPolicy::source_key(ip("2001:db8:1:2:ffff:ffff:ffff:ffff"))
        );
        assert_ne!(
            AddressPolicy::source_key(ip("2001:db8:1:2::1")),
            AddressPolicy::source_key(ip("2001:db8:1:3::1"))
        );
    }

    #[test]
    fn client_key_is_the_ipv4_address_or_the_ipv6_slash_48() {
        assert_eq!(
            AddressPolicy::client_key(ip("203.0.113.9")),
            AddressPolicy::client_key(ip("::ffff:203.0.113.9"))
        );
        assert_ne!(
            AddressPolicy::client_key(ip("203.0.113.9")),
            AddressPolicy::client_key(ip("203.0.113.10"))
        );
        assert_eq!(
            AddressPolicy::client_key(ip("2001:db8:1:2::1")),
            AddressPolicy::client_key(ip("2001:db8:1:ff00::1")),
            "every /64 of one /48 is one client"
        );
        assert_ne!(
            AddressPolicy::client_key(ip("2001:db8:1::1")),
            AddressPolicy::client_key(ip("2001:db8:2::1"))
        );
    }
}
