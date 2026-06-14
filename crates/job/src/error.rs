use thiserror::Error;

#[derive(Debug, Error)]
pub enum JobError {
    #[error("job not found: {0}")]
    NotFound(String),

    #[error("invalid state transition: {0}")]
    InvalidTransition(String),

    #[error("job storage error: {0}")]
    Storage(String),

    /// Generic wrapper for unexpected lower-layer errors that don't
    /// map onto a richer variant (typically libsql driver failures).
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
