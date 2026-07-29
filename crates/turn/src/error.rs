use thiserror::Error;

#[derive(Debug, Error)]
pub enum TurnError {
    #[error("turn not found: {0}")]
    NotFound(String),

    #[error("invalid state transition: {0}")]
    InvalidTransition(String),

    #[error("turn storage error: {0}")]
    Storage(String),

    /// Generic wrapper for unexpected lower-layer errors that don't
    /// map onto a richer variant (typically sqlite driver failures).
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
