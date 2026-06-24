use std::net::SocketAddr;
use std::time::Duration;

use baybo_config::GatewayConfig;

use crate::{GatewayError, Result};

/// Runtime-resolved gateway configuration. Thin wrapper over
/// [`baybo_config::GatewayConfig`] that resolves the socket address up
/// front.
#[derive(Debug, Clone)]
pub struct RuntimeGatewayConfig {
    pub admin_bind: SocketAddr,
    pub cors_allowed_origins: Vec<String>,
    pub shutdown_grace: Duration,
    /// Resolved relay-pairing config — `Some` only when the `relay` block is
    /// enabled with a non-empty URL. Drives the relay-pairing host manager.
    pub relay: Option<RelayPairRuntime>,
}

/// The bits the relay-pairing host manager needs (resolved from the `relay`
/// block once at startup).
#[derive(Debug, Clone)]
pub struct RelayPairRuntime {
    pub url: String,
    pub instance_key: String,
}

impl RuntimeGatewayConfig {
    pub fn from_config(config: &GatewayConfig) -> Result<Self> {
        let addr = format!("{}:{}", config.bind_address, config.port)
            .parse::<SocketAddr>()
            .map_err(|e| GatewayError::Bind {
                addr: format!("{}:{}", config.bind_address, config.port),
                reason: format!("invalid socket address: {e}"),
            })?;
        let relay = (config.relay.enabled && !config.relay.url.is_empty()).then(|| {
            RelayPairRuntime {
                url: config.relay.url.clone(),
                instance_key: config.relay.instance_key.clone(),
            }
        });
        Ok(Self {
            admin_bind: addr,
            cors_allowed_origins: config.cors_allowed_origins.clone(),
            shutdown_grace: Duration::from_secs(config.shutdown_grace_secs),
            relay,
        })
    }
}
