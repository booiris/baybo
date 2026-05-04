use thiserror::Error;

#[derive(Debug, Error)]
pub enum TraceError {
    #[error("trace entity not found: {0}")]
    NotFound(String),

    #[error("trace storage error: {0}")]
    Storage(String),

    #[error("invalid trace operation: {0}")]
    InvalidOperation(String),

    /// Generic wrapper for unexpected lower-layer errors that don't
    /// map onto a richer variant (typically libsql driver failures).
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
