//! Error surface for the mobile client core.

use aura_device_proto::ProtoError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MobileError {
    /// A pairing/crypto failure from the shared protocol.
    #[error("proto: {0}")]
    Proto(#[from] ProtoError),

    /// The handshake was driven out of order (a programming error in the shell).
    #[error("protocol state: {0}")]
    State(&'static str),
}
