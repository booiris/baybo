use std::net::{SocketAddr, SocketAddrV4};

use remote_host_protocol::error::RendezvousAddressError;
use thiserror::Error;

/// Errors from assembling/running the relay service.
#[derive(Debug, Error)]
pub enum RelayError {
    #[error("relay config: {0}")]
    Config(String),
}

/// Why C's UDP rendezvous cannot start. Each one fails startup.
#[derive(Debug, Error)]
pub enum RendezvousStartError {
    #[error("public address: {0}")]
    PublicAddress(#[from] RendezvousAddressError),
    #[error("bind address is not a socket address: {reason}")]
    BindSyntax { reason: String },
    #[error(
        "bind address {address} cannot receive IPv4; the rendezvous observes IPv4 mappings only"
    )]
    BindNotIpv4 { address: SocketAddr },
    #[error("bind {address}: {reason}")]
    Bind {
        address: SocketAddrV4,
        reason: String,
    },
}
