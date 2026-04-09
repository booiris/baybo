use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("memory storage error: {0}")]
    Storage(String),

    #[error("memory not found: {0}")]
    NotFound(String),

    #[error("embedding error: {0}")]
    Embedding(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
