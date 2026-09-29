//! What a leg rides on, shared by A's link table and P's state.

use std::net::IpAddr;

use remote_host_protocol::relay::{AddressClass, AddressPolicy};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CarrierKind {
    /// C's WSS splice.
    Relay,
    /// QUIC to A's private IPv4 address from a private one, or to A's ULA.
    Lan,
    /// QUIC to A's IPv6 global unicast address.
    Ipv6,
    /// QUIC to A's public IPv4 interface address.
    Ipv4,
    /// QUIC through A's IPv4 NAT mapping, opened by a punch.
    Ipv4Punched,
    /// A proven TCP address.
    Tcp,
}

impl CarrierKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Relay => "relay",
            Self::Lan => "lan",
            Self::Ipv6 => "ipv6",
            Self::Ipv4 => "ipv4",
            Self::Ipv4Punched => "ipv4_punched",
            Self::Tcp => "tcp",
        }
    }

    /// The kind of a QUIC carrier from A's local address and P's address, as
    /// A sees them. It names the A candidate P dialed, as P's own label does:
    /// A's local address, except that a private IPv4 address seen from a
    /// public one was reached through A's NAT mapping. IPv6 has no NAT, so
    /// A's address alone decides: `Lan` for a ULA, `Ipv6` for a GUA. IPv4 is
    /// `Ipv4` when A's address is public, `Lan` when both are private, and
    /// `Ipv4Punched` when A's is private and P's public. `None` for a pair
    /// with an excluded address or of mixed families. A hairpinned punch is
    /// the one pair A labels differently from P: P dialed A's mapping, but
    /// the router loops it back from a private source, so A says `Lan`.
    pub fn of_quic_path(gateway: IpAddr, device: IpAddr, policy: &AddressPolicy) -> Option<Self> {
        let gateway = AddressPolicy::canonical_ip(gateway);
        let device = AddressPolicy::canonical_ip(device);
        let classes = (policy.classify(gateway)?, policy.classify(device)?);
        match (gateway, device) {
            (IpAddr::V6(_), IpAddr::V6(_)) => Some(match classes.0 {
                AddressClass::Lan => Self::Lan,
                AddressClass::Public => Self::Ipv6,
            }),
            (IpAddr::V4(_), IpAddr::V4(_)) => Some(match classes {
                (AddressClass::Public, _) => Self::Ipv4,
                (AddressClass::Lan, AddressClass::Lan) => Self::Lan,
                (AddressClass::Lan, AddressClass::Public) => Self::Ipv4Punched,
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(gateway: &str, device: &str) -> Option<CarrierKind> {
        CarrierKind::of_quic_path(
            gateway.parse().unwrap(),
            device.parse().unwrap(),
            &AddressPolicy::active(),
        )
    }

    #[test]
    fn a_quic_carrier_is_classified_by_its_address_pair() {
        assert_eq!(kind("192.168.1.2", "192.168.1.3"), Some(CarrierKind::Lan));
        assert_eq!(kind("fd00::2", "fd00::3"), Some(CarrierKind::Lan));
        assert_eq!(
            kind("2606:4700::2", "2a00:1450::3"),
            Some(CarrierKind::Ipv6)
        );
        assert_eq!(kind("192.168.1.2", "10.0.0.3"), Some(CarrierKind::Lan));
        assert_eq!(kind("8.8.8.8", "1.1.1.1"), Some(CarrierKind::Ipv4));
        assert_eq!(kind("8.8.8.8", "10.0.0.3"), Some(CarrierKind::Ipv4));
        assert_eq!(
            kind("192.168.1.2", "1.1.1.1"),
            Some(CarrierKind::Ipv4Punched)
        );
        assert_eq!(
            kind("::ffff:192.168.1.2", "::ffff:1.1.1.1"),
            Some(CarrierKind::Ipv4Punched)
        );
    }

    #[test]
    fn an_ipv6_carrier_takes_the_class_of_the_address_p_dialed() {
        assert_eq!(kind("fd00::2", "2a00:1450::3"), Some(CarrierKind::Lan));
        assert_eq!(kind("2606:4700::2", "fd00::3"), Some(CarrierKind::Ipv6));
    }

    #[test]
    fn an_excluded_or_mixed_pair_has_no_kind() {
        assert_eq!(kind("0.0.0.0", "1.1.1.1"), None);
        assert_eq!(kind("fd00::2", "fe80::1"), None);
        assert_eq!(kind("192.168.1.2", "fe80::1"), None);
        assert_eq!(kind("8.8.8.8", "2606:4700::2"), None);
    }

    #[test]
    fn every_kind_has_its_tier_label() {
        let labels = [
            CarrierKind::Relay,
            CarrierKind::Lan,
            CarrierKind::Ipv6,
            CarrierKind::Ipv4,
            CarrierKind::Ipv4Punched,
            CarrierKind::Tcp,
        ]
        .map(CarrierKind::as_str);
        assert_eq!(
            labels,
            ["relay", "lan", "ipv6", "ipv4", "ipv4_punched", "tcp"]
        );
    }
}
