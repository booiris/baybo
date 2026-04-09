use thiserror::Error;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session not found: {0}")]
    NotFound(String),

    #[error("session storage error: {0}")]
    Storage(String),

    #[error("session state error: {0}")]
    InvalidState(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
