use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("storage error: {0}")]
    Storage(String),

    #[error("not found: {0}")]
    NotFound(String),

    /// Streaming put exceeded the caller-supplied byte cap. Surfaces as
    /// HTTP 413 at the gateway boundary; internal callers can also use
    /// it to short-circuit oversized writes without buffering the rest.
    #[error("payload too large: {actual} bytes exceeds limit of {limit}")]
    TooLarge { limit: u64, actual: u64 },

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
