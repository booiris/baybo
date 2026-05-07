use thiserror::Error;

#[derive(Debug, Error)]
pub enum ContextError {
    #[error("context compression error: {0}")]
    Compression(String),

    #[error("context snapshot error: {0}")]
    Snapshot(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
