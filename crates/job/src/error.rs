use thiserror::Error;

#[derive(Debug, Error)]
pub enum JobError {
    #[error("job not found: {0}")]
    NotFound(String),

    #[error("invalid state transition: {0}")]
    InvalidTransition(String),

    #[error("job storage error: {0}")]
    Storage(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
