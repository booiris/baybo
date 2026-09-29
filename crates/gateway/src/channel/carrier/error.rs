use device_proto::ProtoError;

/// Why a binding scope has no candidate keys, and so no carrier runtime.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CarrierBindingError {
    #[error("load the gateway static key: {reason}")]
    StaticKey { reason: String },
    #[error("the device public key is {len} bytes, not {expected}")]
    DevicePublicKey { len: usize, expected: usize },
    #[error("derive the candidate keys: {0}")]
    Sealer(#[from] ProtoError),
}

impl CarrierBindingError {
    /// Whether the failure may pass on a retry: reading the gateway's static
    /// key from the vault, unlike a device key that is malformed or
    /// low-order.
    pub(crate) fn is_transient(&self) -> bool {
        matches!(self, Self::StaticKey { .. })
    }
}
