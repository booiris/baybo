use thiserror::Error;

/// Errors from assembling/running the relay service.
#[derive(Debug, Error)]
pub enum RelayError {
    #[error("relay config: {0}")]
    Config(String),
}
