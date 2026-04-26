//! Outbound HTTP/IP allowlist primitives.
//!
//! `is_blocked_ip` is the SSRF floor: it rejects literal IPs in loopback,
//! private (RFC1918, CGNAT), link-local (covers cloud metadata IPs at
//! 169.254/16 and fe80::/10), unique-local, multicast, broadcast,
//! documentation, and reserved ranges, plus IPv4-mapped IPv6 of any of the
//! above. Callers compose this with a custom `reqwest::dns::Resolve` to
//! filter post-resolution addresses (preventing DNS-rebind / public-DNS-
//! pointing-at-private attacks), and with an early literal-IP check on the
//! requested URL for fast-fail UX.
//!
//! `allow_loopback` is a test-only escape hatch — production callers must
//! pass `false`. When `true`, `127.0.0.0/8` and `::1` survive so unit
//! tests can talk to a server bound to 127.0.0.1.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub fn is_blocked_ip(addr: &IpAddr, allow_loopback: bool) -> bool {
    match addr {
        IpAddr::V4(v4) => is_blocked_v4(v4, allow_loopback),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_blocked_v4(&mapped, allow_loopback);
            }
            is_blocked_v6(v6, allow_loopback)
        }
    }
}

fn is_blocked_v4(v4: &Ipv4Addr, allow_loopback: bool) -> bool {
    if v4.is_loopback() {
        return !allow_loopback;
    }
    if v4.is_private() || v4.is_link_local() || v4.is_unspecified() {
        return true;
    }
    if v4.is_broadcast() || v4.is_multicast() || v4.is_documentation() {
        return true;
    }
    let octets = v4.octets();
    if octets[0] == 100 && (64..=127).contains(&octets[1]) {
        return true;
    }
    if octets[0] >= 240 {
        return true;
    }
    false
}

fn is_blocked_v6(v6: &Ipv6Addr, allow_loopback: bool) -> bool {
    if v6.is_loopback() {
        return !allow_loopback;
    }
    if v6.is_unspecified() || v6.is_multicast() {
        return true;
    }
    let segs = v6.segments();
    if (segs[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    if (segs[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn v4(s: &str) -> IpAddr {
        IpAddr::V4(s.parse().unwrap())
    }
    fn v6(s: &str) -> IpAddr {
        IpAddr::V6(s.parse().unwrap())
    }

    #[test]
    fn loopback_v4_blocked_strict_allowed_lax() {
        assert!(is_blocked_ip(&v4("127.0.0.1"), false));
        assert!(is_blocked_ip(&v4("127.1.2.3"), false));
        assert!(!is_blocked_ip(&v4("127.0.0.1"), true));
    }

    #[test]
    fn loopback_v6_blocked_strict_allowed_lax() {
        assert!(is_blocked_ip(&v6("::1"), false));
        assert!(!is_blocked_ip(&v6("::1"), true));
    }

    #[test]
    fn rfc1918_always_blocked() {
        for ip in [
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.0.1",
            "192.168.0.1",
        ] {
            assert!(is_blocked_ip(&v4(ip), false), "{ip}");
            assert!(
                is_blocked_ip(&v4(ip), true),
                "loopback hatch must not unblock {ip}"
            );
        }
    }

    #[test]
    fn link_local_v4_blocked() {
        assert!(is_blocked_ip(&v4("169.254.0.1"), false));
        assert!(is_blocked_ip(&v4("169.254.169.254"), false));
    }

    #[test]
    fn link_local_v6_blocked() {
        assert!(is_blocked_ip(&v6("fe80::1"), false));
        assert!(is_blocked_ip(&v6("febf:ffff::1"), false));
    }

    #[test]
    fn unique_local_v6_blocked() {
        assert!(is_blocked_ip(&v6("fc00::1"), false));
        assert!(is_blocked_ip(&v6("fd00::1"), false));
    }

    #[test]
    fn cgnat_blocked() {
        assert!(is_blocked_ip(&v4("100.64.0.1"), false));
        assert!(is_blocked_ip(&v4("100.127.255.255"), false));
        assert!(!is_blocked_ip(&v4("100.63.255.255"), false));
        assert!(!is_blocked_ip(&v4("100.128.0.0"), false));
    }

    #[test]
    fn unspecified_blocked() {
        assert!(is_blocked_ip(&v4("0.0.0.0"), false));
        assert!(is_blocked_ip(&v6("::"), false));
    }

    #[test]
    fn ipv4_mapped_v6_uses_v4_rules() {
        assert!(is_blocked_ip(&v6("::ffff:127.0.0.1"), false));
        assert!(!is_blocked_ip(&v6("::ffff:127.0.0.1"), true));
        assert!(is_blocked_ip(&v6("::ffff:10.0.0.1"), false));
        assert!(!is_blocked_ip(&v6("::ffff:1.1.1.1"), false));
    }

    #[test]
    fn reserved_v4_blocked() {
        assert!(is_blocked_ip(
            &IpAddr::V4(Ipv4Addr::new(240, 0, 0, 0)),
            false
        ));
        assert!(is_blocked_ip(
            &IpAddr::V4(Ipv4Addr::new(255, 255, 255, 255)),
            false
        ));
    }

    #[test]
    fn public_addresses_pass() {
        assert!(!is_blocked_ip(&v4("1.1.1.1"), false));
        assert!(!is_blocked_ip(&v4("8.8.8.8"), false));
        assert!(!is_blocked_ip(
            &IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0, 0, 0, 0, 0, 0x1111)),
            false
        ));
    }
}
