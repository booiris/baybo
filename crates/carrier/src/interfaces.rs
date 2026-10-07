//! The host's interface addresses: `getifaddrs` for the addresses and
//! interface flags, plus each IPv6 address's own flags (`/proc/net/if_inet6`
//! on Linux, `SIOCGIFAFLAG_IN6` on Apple platforms). The gateway reads them
//! once per accepted offer, the phone once per probe; each applies its own
//! rule to [`InterfaceAddress::temporary`].

use std::ffi::CStr;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// One address of one interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceAddress {
    pub interface: String,
    pub ip: IpAddr,
    /// The interface is UP and RUNNING.
    pub up_running: bool,
    /// An IPv6 privacy address. The gateway never offers one, since it
    /// rotates; the phone does, because iOS sends from one.
    pub temporary: bool,
    /// An IPv6 address the host may drop or has not confirmed: deprecated,
    /// tentative or duplicate. Also set when its flags could not be read.
    pub unusable: bool,
}

/// Every address of every interface. A failure is logged and reads as no
/// address, so the side offers no host candidate.
pub fn enumerate() -> Vec<InterfaceAddress> {
    let raw = match raw_addresses() {
        Ok(raw) => raw,
        Err(error) => {
            tracing::warn!(%error, "carrier: interface addresses unreadable; no host candidate offered");
            return Vec::new();
        }
    };
    let ipv6_flags = platform::Ipv6Flags::read();
    raw.into_iter()
        .map(|raw| {
            let state = match raw.ip {
                IpAddr::V4(_) => Ipv6State::default(),
                IpAddr::V6(ip) => ipv6_flags.state(&raw.interface, ip),
            };
            InterfaceAddress {
                interface: raw.interface,
                ip: raw.ip,
                up_running: raw.up_running,
                temporary: state.temporary,
                unusable: state.unusable,
            }
        })
        .collect()
}

/// What an IPv6 address's own flags say about it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Ipv6State {
    temporary: bool,
    unusable: bool,
}

impl Ipv6State {
    /// The state of an address whose flags could not be read.
    const UNREADABLE: Self = Self {
        temporary: false,
        unusable: true,
    };
}

struct RawAddress {
    interface: String,
    ip: IpAddr,
    up_running: bool,
}

fn raw_addresses() -> io::Result<Vec<RawAddress>> {
    let mut head = std::ptr::null_mut::<libc::ifaddrs>();
    // SAFETY: `getifaddrs` fills `head` with a list it owns until the
    // matching `freeifaddrs` below.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let up_running = (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_uint;
    let mut addresses = Vec::new();
    let mut current = head;
    while !current.is_null() {
        // SAFETY: `current` is a node of the list `getifaddrs` returned, which
        // stays valid until `freeifaddrs`.
        let entry = unsafe { &*current };
        current = entry.ifa_next;
        if entry.ifa_addr.is_null() || entry.ifa_name.is_null() {
            continue;
        }
        // SAFETY: `ifa_addr` is non-null and points at a sockaddr whose
        // `sa_family` selects its concrete layout, checked before each cast.
        let ip = unsafe {
            match i32::from((*entry.ifa_addr).sa_family) {
                libc::AF_INET => {
                    let address = &*(entry.ifa_addr.cast::<libc::sockaddr_in>());
                    Some(IpAddr::V4(Ipv4Addr::from(
                        address.sin_addr.s_addr.to_ne_bytes(),
                    )))
                }
                libc::AF_INET6 => {
                    let address = &*(entry.ifa_addr.cast::<libc::sockaddr_in6>());
                    Some(IpAddr::V6(Ipv6Addr::from(address.sin6_addr.s6_addr)))
                }
                _ => None,
            }
        };
        let Some(ip) = ip else {
            continue;
        };
        // SAFETY: `ifa_name` is a non-null NUL-terminated interface name.
        let interface = unsafe { CStr::from_ptr(entry.ifa_name) }
            .to_string_lossy()
            .into_owned();
        addresses.push(RawAddress {
            interface,
            ip,
            up_running: entry.ifa_flags & up_running == up_running,
        });
    }
    // SAFETY: `head` came from the successful `getifaddrs` above and is freed
    // once; no reference into the list outlives this call.
    unsafe { libc::freeifaddrs(head) };
    Ok(addresses)
}

#[cfg(target_os = "linux")]
mod platform {
    use std::collections::HashMap;
    use std::net::Ipv6Addr;

    use super::Ipv6State;

    const IF_INET6: &str = "/proc/net/if_inet6";
    const TEMPORARY: u32 = libc::IFA_F_TEMPORARY;
    const UNUSABLE: u32 = libc::IFA_F_DEPRECATED | libc::IFA_F_TENTATIVE | libc::IFA_F_DADFAILED;
    const HEX: u32 = 16;

    /// Each IPv6 address's `IFA_F_*` flags, keyed by interface and address.
    pub(super) struct Ipv6Flags(HashMap<(String, Ipv6Addr), u32>);

    impl Ipv6Flags {
        /// No readable table (a kernel without IPv6) marks every IPv6
        /// address unusable.
        pub(super) fn read() -> Self {
            let table = std::fs::read_to_string(IF_INET6).unwrap_or_default();
            Self(parse(&table))
        }

        pub(super) fn state(&self, interface: &str, ip: Ipv6Addr) -> Ipv6State {
            match self.0.get(&(interface.to_owned(), ip)) {
                Some(flags) => Ipv6State {
                    temporary: flags & TEMPORARY != 0,
                    unusable: flags & UNUSABLE != 0,
                },
                None => Ipv6State::UNREADABLE,
            }
        }
    }

    /// Lines of `address(32 hex) ifindex prefix_len scope flags name`, all
    /// hex but the name.
    fn parse(table: &str) -> HashMap<(String, Ipv6Addr), u32> {
        table
            .lines()
            .filter_map(|line| {
                let fields: Vec<&str> = line.split_whitespace().collect();
                let [address, _index, _prefix_len, _scope, flags, name] = fields.as_slice() else {
                    return None;
                };
                let address = u128::from_str_radix(address, HEX).ok()?;
                let flags = u32::from_str_radix(flags, HEX).ok()?;
                Some((((*name).to_owned(), Ipv6Addr::from(address)), flags))
            })
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_ipv6_table_separates_temporary_from_unusable() {
            let table = "\
20010db8000000000000000000000001 02 40 00 80 eth0
20010db8000000000000000000000002 02 40 00 01 eth0
20010db8000000000000000000000003 02 40 00 20 eth0
20010db8000000000000000000000004 02 40 00 40 eth0
20010db8000000000000000000000005 02 40 00 08 eth0
malformed line
";
            let flags = Ipv6Flags(parse(table));
            let ip = |last: u16| Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, last);
            let stable = Ipv6State::default();
            assert_eq!(flags.state("eth0", ip(1)), stable);
            assert_eq!(
                flags.state("eth0", ip(2)),
                Ipv6State {
                    temporary: true,
                    unusable: false
                }
            );
            for unusable in 3..=5 {
                assert!(
                    flags.state("eth0", ip(unusable)).unusable,
                    "address {unusable}"
                );
            }
            assert_eq!(
                flags.state("eth1", ip(1)),
                Ipv6State::UNREADABLE,
                "another interface's entry"
            );
            assert_eq!(
                flags.state("eth0", ip(6)),
                Ipv6State::UNREADABLE,
                "an address the table lacks"
            );
        }
    }
}

#[cfg(target_vendor = "apple")]
mod platform {
    use std::net::Ipv6Addr;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use super::Ipv6State;

    const IOC_INOUT: libc::c_ulong = 0xc000_0000;
    const IOCPARM_MASK: libc::c_ulong = 0x1fff;
    const IOC_GROUP_IF: libc::c_ulong = b'i' as libc::c_ulong;
    const IOC_NUMBER_GIFAFLAG_IN6: libc::c_ulong = 73;
    /// `_IOWR('i', 73, struct in6_ifreq)` from `<netinet6/in6_var.h>`.
    const SIOCGIFAFLAG_IN6: libc::c_ulong = IOC_INOUT
        | ((size_of::<libc::in6_ifreq>() as libc::c_ulong & IOCPARM_MASK) << 16)
        | (IOC_GROUP_IF << 8)
        | IOC_NUMBER_GIFAFLAG_IN6;
    /// The value `<netinet6/in6_var.h>` gives it, so a layout change in
    /// `libc::in6_ifreq` fails the build instead of every flag read.
    const SIOCGIFAFLAG_IN6_IN_SDK: libc::c_ulong = 0xc120_6949;
    const _: () = assert!(SIOCGIFAFLAG_IN6 == SIOCGIFAFLAG_IN6_IN_SDK);
    const IN6_IFF_TENTATIVE: libc::c_int = 0x02;
    const IN6_IFF_DUPLICATED: libc::c_int = 0x04;
    const IN6_IFF_DEPRECATED: libc::c_int = 0x10;
    const IN6_IFF_TEMPORARY: libc::c_int = 0x80;
    const UNUSABLE: libc::c_int = IN6_IFF_TENTATIVE | IN6_IFF_DUPLICATED | IN6_IFF_DEPRECATED;

    /// An `AF_INET6` socket to ask each address's flags on.
    pub(super) struct Ipv6Flags(Option<OwnedFd>);

    impl Ipv6Flags {
        pub(super) fn read() -> Self {
            // SAFETY: a plain socket(2) call; a valid descriptor is owned below.
            let fd = unsafe { libc::socket(libc::AF_INET6, libc::SOCK_DGRAM, 0) };
            // SAFETY: `fd` is a fresh descriptor this call owns.
            Self((fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) }))
        }

        /// An address whose flags cannot be read counts as unusable.
        pub(super) fn state(&self, interface: &str, ip: Ipv6Addr) -> Ipv6State {
            let Some(socket) = &self.0 else {
                return Ipv6State::UNREADABLE;
            };
            let name = interface.as_bytes();
            // SAFETY: `in6_ifreq` is plain old data, valid when zeroed.
            let mut request: libc::in6_ifreq = unsafe { std::mem::zeroed() };
            if name.len() >= request.ifr_name.len() {
                return Ipv6State::UNREADABLE;
            }
            for (slot, byte) in request.ifr_name.iter_mut().zip(name) {
                *slot = *byte as libc::c_char;
            }
            // SAFETY: zeroed `sockaddr_in6` is valid; the fields set below
            // make it the address whose flags are asked for.
            let mut address: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            address.sin6_len = size_of::<libc::sockaddr_in6>() as u8;
            address.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            address.sin6_addr.s6_addr = ip.octets();
            request.ifr_ifru.ifru_addr = address;
            // SAFETY: `request` is a valid `in6_ifreq` for SIOCGIFAFLAG_IN6,
            // which writes only into it.
            let status = unsafe {
                libc::ioctl(
                    socket.as_raw_fd(),
                    SIOCGIFAFLAG_IN6,
                    &mut request as *mut libc::in6_ifreq,
                )
            };
            if status != 0 {
                return Ipv6State::UNREADABLE;
            }
            // SAFETY: on success the kernel filled the `ifru_flags6` member.
            let flags = unsafe { request.ifr_ifru.ifru_flags6 };
            Ipv6State {
                temporary: flags & IN6_IFF_TEMPORARY != 0,
                unusable: flags & UNUSABLE != 0,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_enumerates_its_loopback_as_up_and_running() {
        let addresses = enumerate();
        assert!(
            addresses
                .iter()
                .any(|address| address.ip == IpAddr::V4(Ipv4Addr::LOCALHOST) && address.up_running),
            "{addresses:?}"
        );
    }

    /// Reads the IPv6 flags through the platform's own path: a wrong ioctl
    /// request or table parse marks every IPv6 address unusable. macOS always
    /// has `::1`; a Linux host with IPv6 disabled has none and skips.
    #[test]
    fn the_ipv6_loopback_reads_as_a_stable_address() {
        let addresses = enumerate();
        let Some(loopback) = addresses
            .iter()
            .find(|address| address.ip == IpAddr::V6(Ipv6Addr::LOCALHOST))
        else {
            if cfg!(target_os = "macos") {
                panic!("macOS lists ::1 on lo0: {addresses:?}");
            }
            eprintln!("skipping: no IPv6 loopback on this host");
            return;
        };
        assert!(loopback.up_running, "{loopback:?}");
        assert!(!loopback.unusable && !loopback.temporary, "{loopback:?}");
    }
}
