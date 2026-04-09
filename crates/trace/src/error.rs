use thiserror::Error;

#[derive(Debug, Error)]
pub enum TraceError {
    #[error("trace not found: {0}")]
    NotFound(String),

    #[error("trace storage error: {0}")]
    Storage(String),

    #[error("invalid trace operation: {0}")]
    InvalidOperation(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
