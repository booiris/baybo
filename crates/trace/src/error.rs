use thiserror::Error;

#[derive(Debug, Error)]
pub enum TraceError {
    #[error("trace entity not found: {0}")]
    NotFound(String),

    #[error("trace storage error: {0}")]
    Storage(String),

    #[error("invalid trace operation: {0}")]
    InvalidOperation(String),

    /// Returned by `Span::end_ok` / `end_failed` / `end_cancelled` when
    /// the span has already been ended.
    #[error("span already ended: {0}")]
    AlreadyEnded(String),

    /// Generic wrapper for unexpected lower-layer errors that don't
    /// map onto a richer variant (typically libsql driver failures).
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
