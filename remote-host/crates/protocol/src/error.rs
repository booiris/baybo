//! Error types of the protocol crate's parsers.

use thiserror::Error;

/// Why a UDP rendezvous address was rejected. The variants never carry the
/// address itself: callers log them at info level, where addresses must not
/// appear.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RendezvousAddressError {
    #[error("rendezvous address is empty or too long")]
    Length,
    #[error("rendezvous address has no valid non-zero port")]
    Port,
    #[error("rendezvous host is neither an IPv4 literal nor an LDH hostname")]
    Host,
    #[error("rendezvous literal is not a public IPv4 address")]
    NotPublicLiteral,
    #[error("rendezvous lookup failed: {reason}")]
    Lookup { reason: String },
    #[error("rendezvous name has no IPv4 address")]
    NoIpv4,
    #[error("rendezvous name resolves to a non-public IPv4 address")]
    NotPublic,
}

/// Why a datagram did not decode as a [`crate::relay::ProbeDatagram`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProbeDecodeError {
    #[error("datagram does not start with the probe magic")]
    NotProbe,
    #[error("unknown probe datagram kind {kind}")]
    UnknownKind { kind: u8 },
    #[error("probe datagram kind {kind} is {len} bytes, expected {expected}")]
    Length {
        kind: u8,
        len: usize,
        expected: usize,
    },
    #[error("register datagram names unknown role {role}")]
    Role { role: u8 },
    #[error("register datagram padding is not zero")]
    Padding,
}
