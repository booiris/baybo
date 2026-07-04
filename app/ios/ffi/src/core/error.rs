//! Error surface for the iOS client protocol core.

use device_proto::ProtoError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MobileError {
    #[error("proto: {0}")]
    Proto(#[from] ProtoError),

    #[error("noise: {0}")]
    Noise(#[from] snow::Error),

    #[error("wire: {0}")]
    Wire(#[from] wire::WireError),

    #[error("protocol state: {0}")]
    State(&'static str),
}
