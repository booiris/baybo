//! P's view of its network. The primary interface's fingerprint decides
//! whether a path update retires the carrier; the network key scopes the
//! failure cache; the local path says which tiers P may offer and dial.

use std::net::IpAddr;

use carrier::interfaces::InterfaceAddress;
use remote_host_protocol::relay::{AddressClass, AddressPolicy};
use sha2::{Digest, Sha256};

use crate::api::{NetworkInterfaceKind, NetworkPath};

pub(crate) const NETWORK_KEY_LEN: usize = 8;
const NETWORK_KEY_DOMAIN: &[u8] = b"baybo/direct/network/v1";

/// The primary interface: its kind, name and gateways. A path update that
/// keeps it equal keeps the carrier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrimaryInterface {
    kind: NetworkInterfaceKind,
    name: String,
    gateways: Vec<String>,
}

impl PrimaryInterface {
    pub(crate) fn of(path: &NetworkPath) -> Self {
        let mut gateways = path.gateways.clone();
        gateways.sort();
        gateways.dedup();
        Self {
            kind: path.interface_kind,
            name: path.interface_name.clone(),
            gateways,
        }
    }
}

/// The failure cache's network: the primary interface kind, and on Wi-Fi or
/// wired the interface's IPv4 /24s and the path's gateways. A cellular path is
/// keyed by its kind alone: its IPv6 prefix and CGNAT address change on every
/// re-attach and would reset the backoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct NetworkKey([u8; NETWORK_KEY_LEN]);

impl NetworkKey {
    pub(crate) fn of(path: &NetworkPath, primary_addresses: &[InterfaceAddress]) -> Self {
        let mut hash = Sha256::new();
        hash.update(NETWORK_KEY_DOMAIN);
        hash.update([kind_byte(path.interface_kind)]);
        if is_local_area(path.interface_kind) {
            let mut prefixes: Vec<[u8; 3]> = primary_addresses
                .iter()
                .filter_map(|address| match address.ip {
                    IpAddr::V4(ip) => {
                        let [a, b, c, _] = ip.octets();
                        Some([a, b, c])
                    }
                    IpAddr::V6(_) => None,
                })
                .collect();
            prefixes.sort_unstable();
            prefixes.dedup();
            hash.update(len_prefix(prefixes.len()));
            for prefix in &prefixes {
                hash.update(prefix);
            }
            let PrimaryInterface { gateways, .. } = PrimaryInterface::of(path);
            hash.update(len_prefix(gateways.len()));
            for gateway in &gateways {
                hash.update(len_prefix(gateway.len()));
                hash.update(gateway.as_bytes());
            }
        }
        let digest = hash.finalize();
        let mut key = [0u8; NETWORK_KEY_LEN];
        key.copy_from_slice(&digest[..NETWORK_KEY_LEN]);
        Self(key)
    }

    /// A short label for logs; it says nothing about the network itself.
    pub(crate) fn tag(&self) -> String {
        hex::encode(&self.0[..4])
    }
}

fn len_prefix(len: usize) -> [u8; 8] {
    (len as u64).to_be_bytes()
}

fn kind_byte(kind: NetworkInterfaceKind) -> u8 {
    match kind {
        NetworkInterfaceKind::Wifi => 1,
        NetworkInterfaceKind::Wired => 2,
        NetworkInterfaceKind::Cellular => 3,
        NetworkInterfaceKind::Loopback => 4,
        NetworkInterfaceKind::Other => 5,
    }
}

/// Wi-Fi or wired: the only interfaces whose private addresses a peer on the
/// same network can reach. A private address on cellular is the carrier's
/// CGNAT, which nobody can dial.
pub(crate) fn is_local_area(kind: NetworkInterfaceKind) -> bool {
    matches!(
        kind,
        NetworkInterfaceKind::Wifi | NetworkInterfaceKind::Wired
    )
}

/// The path a probe runs on: the primary interface, its usable addresses, and
/// the families it routes.
#[derive(Debug, Clone)]
pub(crate) struct LocalPath {
    pub(crate) kind: NetworkInterfaceKind,
    pub(crate) addresses: Vec<InterfaceAddress>,
    pub(crate) supports_ipv4: bool,
    pub(crate) supports_ipv6: bool,
}

impl LocalPath {
    /// `interfaces` is every address on the host; only the primary
    /// interface's, up and usable, are kept.
    pub(crate) fn new(path: &NetworkPath, interfaces: &[InterfaceAddress]) -> Self {
        Self {
            kind: path.interface_kind,
            addresses: primary_addresses(path, interfaces),
            supports_ipv4: path.supports_ipv4,
            supports_ipv6: path.supports_ipv6,
        }
    }

    pub(crate) fn is_local_area(&self) -> bool {
        is_local_area(self.kind)
    }

    /// Whether this path may dial A's `Lan`-class candidates of `ip`'s
    /// family: only from Wi-Fi or wired, and only with a `Lan`-class address
    /// of that family of its own. On cellular A's private addresses route
    /// into the mobile carrier; on a foreign Wi-Fi that reuses A's subnet they
    /// would reach unrelated hosts.
    pub(crate) fn may_dial_lan(&self, ip: IpAddr, policy: &AddressPolicy) -> bool {
        self.is_local_area()
            && self.addresses.iter().any(|address| {
                address.ip.is_ipv4() == ip.is_ipv4()
                    && policy.classify(address.ip) == Some(AddressClass::Lan)
            })
    }

    /// Whether `ip` is still one of the primary interface's addresses: a
    /// carrier whose local address left the path is retired.
    pub(crate) fn holds(&self, ip: IpAddr) -> bool {
        let ip = AddressPolicy::canonical_ip(ip);
        ip.is_unspecified()
            || self
                .addresses
                .iter()
                .any(|address| AddressPolicy::canonical_ip(address.ip) == ip)
    }
}

/// The primary interface's addresses that are up and usable: temporary IPv6
/// addresses are kept, since iOS sends from one.
pub(crate) fn primary_addresses(
    path: &NetworkPath,
    interfaces: &[InterfaceAddress],
) -> Vec<InterfaceAddress> {
    interfaces
        .iter()
        .filter(|address| {
            address.interface == path.interface_name && address.up_running && !address.unusable
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(kind: NetworkInterfaceKind, name: &str, gateways: &[&str]) -> NetworkPath {
        NetworkPath {
            satisfied: true,
            interface_kind: kind,
            interface_name: name.to_owned(),
            gateways: gateways.iter().map(|g| (*g).to_owned()).collect(),
            supports_ipv4: true,
            supports_ipv6: true,
            available_interfaces: vec![name.to_owned()],
            is_expensive: false,
            is_constrained: false,
        }
    }

    fn address(interface: &str, ip: &str) -> InterfaceAddress {
        InterfaceAddress {
            interface: interface.to_owned(),
            ip: ip.parse().unwrap(),
            up_running: true,
            temporary: false,
            unusable: false,
        }
    }

    #[test]
    fn the_primary_interface_ignores_gateway_order_and_secondary_interfaces() {
        let mut a = path(
            NetworkInterfaceKind::Wifi,
            "en0",
            &["192.168.1.1", "fe80::1"],
        );
        let mut b = path(
            NetworkInterfaceKind::Wifi,
            "en0",
            &["fe80::1", "192.168.1.1"],
        );
        b.available_interfaces.push("pdp_ip0".into());
        b.is_expensive = true;
        assert_eq!(PrimaryInterface::of(&a), PrimaryInterface::of(&b));
        a.interface_kind = NetworkInterfaceKind::Cellular;
        assert_ne!(PrimaryInterface::of(&a), PrimaryInterface::of(&b));
    }

    #[test]
    fn a_wifi_network_is_keyed_by_its_subnets_and_gateways() {
        let home = path(NetworkInterfaceKind::Wifi, "en0", &["192.168.1.1"]);
        let cafe = path(NetworkInterfaceKind::Wifi, "en0", &["10.0.0.1"]);
        let at_home = [
            address("en0", "192.168.1.20"),
            address("en0", "2001:db8::5"),
        ];
        let new_lease = [address("en0", "192.168.1.77")];
        assert_eq!(
            NetworkKey::of(&home, &at_home),
            NetworkKey::of(&home, &new_lease),
            "a new lease in the same /24 is the same network"
        );
        assert_ne!(
            NetworkKey::of(&home, &at_home),
            NetworkKey::of(&cafe, &[address("en0", "10.0.0.20")])
        );
    }

    #[test]
    fn a_cellular_network_is_keyed_by_its_kind_alone() {
        let one = path(NetworkInterfaceKind::Cellular, "pdp_ip0", &["fe80::1"]);
        let other = path(NetworkInterfaceKind::Cellular, "pdp_ip1", &["fe80::2"]);
        assert_eq!(
            NetworkKey::of(&one, &[address("pdp_ip0", "100.64.3.4")]),
            NetworkKey::of(&other, &[address("pdp_ip1", "100.70.9.9")])
        );
    }

    #[test]
    fn a_cellular_path_never_dials_lan_candidates() {
        let policy = AddressPolicy::active();
        let gateway_lan: IpAddr = "192.168.1.10".parse().unwrap();
        let interfaces = [address("pdp_ip0", "10.20.30.40")];
        let cellular = LocalPath::new(
            &path(NetworkInterfaceKind::Cellular, "pdp_ip0", &[]),
            &interfaces,
        );
        assert!(!cellular.may_dial_lan(gateway_lan, &policy));

        let interfaces = [address("en0", "192.168.1.20")];
        let wifi = LocalPath::new(&path(NetworkInterfaceKind::Wifi, "en0", &[]), &interfaces);
        assert!(wifi.may_dial_lan(gateway_lan, &policy));
        assert!(
            !wifi.may_dial_lan("fd00::10".parse().unwrap(), &policy),
            "a Wi-Fi path with no ULA of its own does not dial A's ULA"
        );
    }

    #[test]
    fn only_the_primary_interfaces_usable_addresses_count() {
        let mut deprecated = address("en0", "2001:db8::9");
        deprecated.unusable = true;
        let mut temporary = address("en0", "2001:db8::7");
        temporary.temporary = true;
        let interfaces = [
            address("en0", "192.168.1.20"),
            address("pdp_ip0", "10.20.30.40"),
            deprecated,
            temporary,
        ];
        let local = LocalPath::new(&path(NetworkInterfaceKind::Wifi, "en0", &[]), &interfaces);
        assert!(local.holds("192.168.1.20".parse().unwrap()));
        assert!(
            local.holds("2001:db8::7".parse().unwrap()),
            "temporary is kept"
        );
        assert!(!local.holds("2001:db8::9".parse().unwrap()));
        assert!(!local.holds("10.20.30.40".parse().unwrap()));
    }
}
