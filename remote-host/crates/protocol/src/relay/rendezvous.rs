//! The UDP rendezvous address: C normalises `UDP_PUBLIC_ADDR` once at startup,
//! and A and P resolve what C sends them. Both go through the address policy.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs};

use super::UdpRendezvous;
use super::address::AddressPolicy;
use crate::error::RendezvousAddressError;

const MAX_HOSTNAME_BYTES: usize = 253;
const MAX_LABEL_BYTES: usize = 63;
const MAX_ADDRESS_BYTES: usize = MAX_HOSTNAME_BYTES + ":65535".len();

enum RendezvousHost {
    Literal(SocketAddrV4),
    Name { host: String, port: u16 },
}

impl UdpRendezvous {
    /// Normalises a configured rendezvous address to `host:port`, where `host`
    /// is a `Public` IPv4 literal or a lowercase LDH hostname without a
    /// trailing dot. A hostname is not resolved here.
    pub fn normalize_address(
        value: &str,
        policy: &AddressPolicy,
    ) -> Result<String, RendezvousAddressError> {
        match parse(value, policy)? {
            RendezvousHost::Literal(address) => Ok(address.to_string()),
            RendezvousHost::Name { host, port } => Ok(format!("{host}:{port}")),
        }
    }

    /// Resolves `address` for sending `Register`s. A literal must be `Public`
    /// IPv4. A hostname is looked up, only its IPv4 (A-record) results are
    /// used, and every one of them must be `Public`; the first is returned.
    ///
    /// Blocks on the system resolver: async callers run it on a blocking
    /// thread.
    pub fn resolve_public_v4(
        &self,
        policy: &AddressPolicy,
    ) -> Result<SocketAddrV4, RendezvousAddressError> {
        match parse(&self.address, policy)? {
            RendezvousHost::Literal(address) => Ok(address),
            RendezvousHost::Name { host, port } => {
                let resolved = (host.as_str(), port).to_socket_addrs().map_err(|error| {
                    RendezvousAddressError::Lookup {
                        reason: error.to_string(),
                    }
                })?;
                public_v4_results(resolved, policy)
            }
        }
    }
}

fn public_v4_results(
    resolved: impl Iterator<Item = SocketAddr>,
    policy: &AddressPolicy,
) -> Result<SocketAddrV4, RendezvousAddressError> {
    let mut first = None;
    for address in resolved {
        let address = AddressPolicy::canonical_socket_addr(address);
        let SocketAddr::V4(address) = address else {
            continue;
        };
        if policy.public_v4(IpAddr::V4(*address.ip())).is_none() {
            return Err(RendezvousAddressError::NotPublic);
        }
        first.get_or_insert(address);
    }
    first.ok_or(RendezvousAddressError::NoIpv4)
}

fn parse(value: &str, policy: &AddressPolicy) -> Result<RendezvousHost, RendezvousAddressError> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_ADDRESS_BYTES {
        return Err(RendezvousAddressError::Length);
    }
    let (host, port) = value.rsplit_once(':').ok_or(RendezvousAddressError::Port)?;
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or(RendezvousAddressError::Port)?;
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return policy
            .public_v4(IpAddr::V4(ip))
            .map(|ip| RendezvousHost::Literal(SocketAddrV4::new(ip, port)))
            .ok_or(RendezvousAddressError::NotPublicLiteral);
    }
    let host = host.strip_suffix('.').unwrap_or(host);
    if !is_ldh_hostname(host) {
        return Err(RendezvousAddressError::Host);
    }
    Ok(RendezvousHost::Name {
        host: host.to_ascii_lowercase(),
        port,
    })
}

/// Letters, digits and hyphens, in labels of 1–63 bytes that neither start nor
/// end with a hyphen. The last label is not all digits, so a resolver never
/// reads the name as a shorthand IPv4 literal such as `127.1`.
fn is_ldh_hostname(host: &str) -> bool {
    let valid_label = |label: &str| {
        let bytes = label.as_bytes();
        (1..=MAX_LABEL_BYTES).contains(&bytes.len())
            && bytes
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
            && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
            && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
    };
    (1..=MAX_HOSTNAME_BYTES).contains(&host.len())
        && host.split('.').all(valid_label)
        && host
            .rsplit('.')
            .next()
            .is_some_and(|last| !last.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::{RENDEZVOUS_KEY_LEN, RendezvousKey};

    fn rendezvous(address: &str) -> UdpRendezvous {
        UdpRendezvous {
            address: address.to_owned(),
            key: RendezvousKey::from_bytes([1; RENDEZVOUS_KEY_LEN]),
        }
    }

    #[test]
    fn normalizes_hostnames_and_public_ipv4_literals() {
        let policy = AddressPolicy::production();
        assert_eq!(
            UdpRendezvous::normalize_address(" Rdv.Example.:7777 ", &policy),
            Ok("rdv.example:7777".to_owned())
        );
        assert_eq!(
            UdpRendezvous::normalize_address("8.8.8.8:7777", &policy),
            Ok("8.8.8.8:7777".to_owned())
        );
    }

    #[test]
    fn rejects_malformed_and_non_public_addresses() {
        let policy = AddressPolicy::production();
        for (value, error) in [
            ("", RendezvousAddressError::Length),
            ("rdv.example", RendezvousAddressError::Port),
            ("rdv.example:0", RendezvousAddressError::Port),
            ("rdv.example:70000", RendezvousAddressError::Port),
            ("127.0.0.1:7777", RendezvousAddressError::NotPublicLiteral),
            ("10.0.0.1:7777", RendezvousAddressError::NotPublicLiteral),
            ("[2001:4860::8888]:7777", RendezvousAddressError::Host),
            ("2001:4860::8888:7777", RendezvousAddressError::Host),
            ("-rdv.example:7777", RendezvousAddressError::Host),
            ("rdv_x.example:7777", RendezvousAddressError::Host),
            ("rdv..example:7777", RendezvousAddressError::Host),
            ("127.1:7777", RendezvousAddressError::Host),
            ("999.1.1.1:7777", RendezvousAddressError::Host),
        ] {
            assert_eq!(
                UdpRendezvous::normalize_address(value, &policy),
                Err(error),
                "{value}"
            );
        }
        let long = format!("{}.example:7777", "a".repeat(MAX_LABEL_BYTES + 1));
        assert_eq!(
            UdpRendezvous::normalize_address(&long, &policy),
            Err(RendezvousAddressError::Host)
        );
    }

    #[test]
    fn production_policy_refuses_a_loopback_or_lan_rendezvous() {
        let policy = AddressPolicy::production();
        assert_eq!(
            rendezvous("127.0.0.1:7777").resolve_public_v4(&policy),
            Err(RendezvousAddressError::NotPublicLiteral)
        );
        assert_eq!(
            rendezvous("192.168.1.10:7777").resolve_public_v4(&policy),
            Err(RendezvousAddressError::NotPublicLiteral)
        );
        assert_eq!(
            public_v4_results(localhost_lookup(), &policy),
            Err(RendezvousAddressError::NotPublic)
        );
        assert_eq!(
            public_v4_results(
                ["192.168.1.10:7777".parse::<SocketAddr>().unwrap()].into_iter(),
                &policy
            ),
            Err(RendezvousAddressError::NotPublic)
        );
        assert_eq!(
            rendezvous("8.8.8.8:7777").resolve_public_v4(&policy),
            Ok("8.8.8.8:7777".parse().unwrap())
        );
    }

    #[test]
    fn test_policy_resolves_loopback() {
        let policy = AddressPolicy::for_tests();
        assert_eq!(
            rendezvous("127.0.0.1:7777").resolve_public_v4(&policy),
            Ok("127.0.0.1:7777".parse().unwrap())
        );
        assert_eq!(
            public_v4_results(localhost_lookup(), &policy),
            Ok("127.0.0.1:7777".parse().unwrap())
        );
    }

    fn localhost_lookup() -> impl Iterator<Item = SocketAddr> {
        ["[::1]:7777", "127.0.0.1:7777"]
            .into_iter()
            .map(|address| address.parse().unwrap())
    }

    #[test]
    fn lookup_results_use_ipv4_only_and_require_every_one_public() {
        let policy = AddressPolicy::production();
        let addr = |value: &str| value.parse::<SocketAddr>().unwrap();
        assert_eq!(
            public_v4_results(
                [
                    addr("[2606:4700::1]:7777"),
                    addr("1.2.3.4:7777"),
                    addr("5.6.7.8:7777")
                ]
                .into_iter(),
                &policy
            ),
            Ok("1.2.3.4:7777".parse().unwrap())
        );
        assert_eq!(
            public_v4_results(
                [addr("1.2.3.4:7777"), addr("10.0.0.1:7777")].into_iter(),
                &policy
            ),
            Err(RendezvousAddressError::NotPublic)
        );
        assert_eq!(
            public_v4_results([addr("[::ffff:1.2.3.4]:7777")].into_iter(), &policy),
            Ok("1.2.3.4:7777".parse().unwrap())
        );
        assert_eq!(
            public_v4_results([addr("[2606:4700::1]:7777")].into_iter(), &policy),
            Err(RendezvousAddressError::NoIpv4)
        );
    }
}
