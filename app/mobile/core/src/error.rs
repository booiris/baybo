//! Error surface for the mobile client core.

use device_proto::ProtoError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MobileError {
    /// A pairing/crypto failure from the shared protocol.
    #[error("proto: {0}")]
    Proto(#[from] ProtoError),

    /// A Noise handshake / transport failure on the content session.
    #[error("noise: {0}")]
    Noise(#[from] snow::Error),

    /// A `Frame` encode/decode failure on the content session.
    #[error("wire: {0}")]
    Wire(#[from] wire::WireError),

    /// The handshake was driven out of order (a programming error in the shell).
    #[error("protocol state: {0}")]
    State(&'static str),
}
