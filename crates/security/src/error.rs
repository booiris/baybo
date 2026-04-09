use thiserror::Error;

#[derive(Debug, Error)]
pub enum SecurityError {
    #[error("security violation: {0}")]
    Violation(String),

    #[error("encryption error: {0}")]
    Encryption(String),

    #[error("secret not found: {0}")]
    NotFound(String),

    #[error("secret storage error: {0}")]
    Storage(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
