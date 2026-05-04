use thiserror::Error;

#[derive(Debug, Error)]
pub enum JobError {
    #[error("job not found: {0}")]
    NotFound(String),

    #[error("invalid state transition: {0}")]
    InvalidTransition(String),

    #[error("verification not allowed: {0}")]
    VerificationNotAllowed(String),

    #[error("invalid verification advance: {0}")]
    InvalidVerificationAdvance(String),

    #[error("kind / input mismatch: {0}")]
    KindMismatch(String),

    #[error("job storage error: {0}")]
    Storage(String),

    /// Generic wrapper for unexpected lower-layer errors that don't
    /// map onto a richer variant (typically libsql driver failures).
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
