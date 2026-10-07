use std::fmt;
use std::net::SocketAddr;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CarrierError {
    #[error("bind the carrier UDP socket on {address}: {reason}")]
    Bind { address: SocketAddr, reason: String },
    #[error("start the QUIC endpoint: {reason}")]
    Endpoint { reason: String },
    #[error("generate the QUIC certificate: {reason}")]
    Certificate { reason: String },
    #[error("build the QUIC {side} TLS configuration: {reason}")]
    Tls { side: TlsSide, reason: String },
}

/// The side whose TLS configuration failed to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsSide {
    Server,
    Client,
}

impl fmt::Display for TlsSide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Server => "server",
            Self::Client => "client",
        })
    }
}

/// Why reading or writing a carrier frame failed. No variant carries frame
/// bytes, since a `DirectOpen` frame holds the direct token.
#[derive(Debug, Error)]
pub enum FrameError {
    #[error("frame of {len} bytes exceeds the {max}-byte cap")]
    TooLarge { len: usize, max: usize },
    #[error("stream ended inside a frame")]
    Truncated,
    #[error("the stream ended before the DirectOpen preface")]
    MissingPreface,
    #[error("malformed DirectOpen preface: {reason}")]
    Preface { reason: String },
    #[error("frame i/o failed: {0}")]
    Io(#[from] std::io::Error),
    /// A `FrameReader` that returned an error has lost its place in the
    /// stream, so every later read fails with this.
    #[error("an earlier frame error ended the stream")]
    Poisoned,
}
