use std::net::{AddrParseError, IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use baybo_config::{DirectTcpConfig, DirectUdpConfig, GatewayConfig};

use crate::{GatewayError, Result};

/// Runtime-resolved gateway configuration. Thin wrapper over
/// [`baybo_config::GatewayConfig`] that resolves the socket address up
/// front.
#[derive(Debug, Clone)]
pub struct RuntimeGatewayConfig {
    pub admin_bind: SocketAddr,
    pub cors_allowed_origins: Vec<String>,
    pub shutdown_grace: Duration,
    pub carrier: RuntimeCarrierConfig,
}

/// Where a relay binding's carrier runtime binds, copied from
/// `gateway.direct_udp` and `gateway.direct_tcp`. `baybo-config` validates
/// their shape, so nothing here checks it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCarrierConfig {
    /// `None` when `gateway.direct_udp.enabled` is false.
    pub udp: Option<FamilyBinds>,
    /// `None` when `gateway.direct_tcp` is absent.
    pub tcp: Option<RuntimeDirectTcpConfig>,
}

/// At most one bind address per address family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FamilyBinds {
    pub ipv4: Option<SocketAddr>,
    pub ipv6: Option<SocketAddr>,
}

impl FamilyBinds {
    fn addresses(&self) -> impl Iterator<Item = SocketAddr> {
        [self.ipv4, self.ipv6].into_iter().flatten()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeDirectTcpConfig {
    pub binds: FamilyBinds,
    pub advertised_addresses: Vec<SocketAddr>,
}

impl RuntimeGatewayConfig {
    pub fn from_config(config: &GatewayConfig) -> Result<Self> {
        let addr = format!("{}:{}", config.bind_address, config.port)
            .parse::<SocketAddr>()
            .map_err(|e| GatewayError::Bind {
                addr: format!("{}:{}", config.bind_address, config.port),
                reason: format!("invalid socket address: {e}"),
            })?;
        Ok(Self {
            admin_bind: addr,
            cors_allowed_origins: config.cors_allowed_origins.clone(),
            shutdown_grace: Duration::from_secs(config.shutdown_grace_secs),
            carrier: RuntimeCarrierConfig::from_config(
                &config.direct_udp,
                config.direct_tcp.as_ref(),
            ),
        })
    }
}

/// Where a client on this host dials the admin listener: the configured bind,
/// with a wildcard (`0.0.0.0` or `::`) rewritten to IPv4 loopback, since a
/// wildcard is a bind directive, not a destination.
pub fn admin_dial_addr(config: &GatewayConfig) -> Result<SocketAddr> {
    let host = config.bind_address.as_str();
    let ip: IpAddr = host
        .parse()
        .map_err(|e: AddrParseError| GatewayError::AdminAddress {
            addr: host.to_owned(),
            reason: e.to_string(),
        })?;
    let dial_ip = if ip.is_unspecified() {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        ip
    };
    Ok(SocketAddr::new(dial_ip, config.port))
}

impl RuntimeCarrierConfig {
    /// The UDP bind addresses, IPv4 first; none while direct UDP is off.
    pub(crate) fn udp_binds(&self) -> Vec<SocketAddr> {
        self.udp.iter().flat_map(FamilyBinds::addresses).collect()
    }

    /// The TCP listener addresses, IPv4 first; none without `direct_tcp`.
    pub(crate) fn tcp_binds(&self) -> Vec<SocketAddr> {
        self.tcp
            .iter()
            .flat_map(|tcp| tcp.binds.addresses())
            .collect()
    }

    /// Whether a binding's carrier runtime has a socket or listener to bind.
    pub(crate) fn binds_anything(&self) -> bool {
        !self.udp_binds().is_empty() || !self.tcp_binds().is_empty()
    }

    fn from_config(udp: &DirectUdpConfig, tcp: Option<&DirectTcpConfig>) -> Self {
        Self {
            udp: udp.enabled.then_some(FamilyBinds {
                ipv4: udp.ipv4_bind,
                ipv6: udp.ipv6_bind,
            }),
            tcp: tcp.map(|tcp| RuntimeDirectTcpConfig {
                binds: FamilyBinds {
                    ipv4: tcp.ipv4_bind,
                    ipv6: tcp.ipv6_bind,
                },
                advertised_addresses: tcp.advertised_addresses.clone(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socket(address: &str) -> SocketAddr {
        address.parse().unwrap()
    }

    #[test]
    fn a_wildcard_admin_bind_is_dialed_on_loopback() {
        let mut gateway = GatewayConfig {
            port: 7788,
            ..GatewayConfig::default()
        };
        for (bind, dialed) in [
            ("0.0.0.0", "127.0.0.1:7788"),
            ("::", "127.0.0.1:7788"),
            ("192.168.1.5", "192.168.1.5:7788"),
            ("::1", "[::1]:7788"),
        ] {
            gateway.bind_address = bind.to_owned();
            assert_eq!(admin_dial_addr(&gateway).unwrap(), socket(dialed), "{bind}");
        }
        gateway.bind_address = "localhost".to_owned();
        assert!(matches!(
            admin_dial_addr(&gateway),
            Err(GatewayError::AdminAddress { .. })
        ));
    }

    #[test]
    fn the_carrier_config_copies_the_direct_sections() {
        let mut gateway = GatewayConfig::default();
        let runtime = RuntimeGatewayConfig::from_config(&gateway).unwrap();
        assert_eq!(
            runtime.carrier,
            RuntimeCarrierConfig {
                udp: Some(FamilyBinds {
                    ipv4: Some(socket("0.0.0.0:0")),
                    ipv6: Some(socket("[::]:0")),
                }),
                tcp: None,
            }
        );

        gateway.direct_udp.enabled = false;
        gateway.direct_tcp = Some(DirectTcpConfig {
            ipv4_bind: Some(socket("0.0.0.0:42124")),
            ipv6_bind: None,
            advertised_addresses: vec![socket("203.0.113.7:42124")],
        });
        let runtime = RuntimeGatewayConfig::from_config(&gateway).unwrap();
        assert_eq!(
            runtime.carrier,
            RuntimeCarrierConfig {
                udp: None,
                tcp: Some(RuntimeDirectTcpConfig {
                    binds: FamilyBinds {
                        ipv4: Some(socket("0.0.0.0:42124")),
                        ipv6: None,
                    },
                    advertised_addresses: vec![socket("203.0.113.7:42124")],
                }),
            }
        );
    }
}
