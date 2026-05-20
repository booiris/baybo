use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("embedding error: {0}")]
    Embedding(String),

    #[error("memory entry {0} not found")]
    NotFound(String),

    /// Wraps a lower-layer storage failure. The libsql `MemoryStore`
    /// implementation stringifies `aura_storage::StorageError` into this
    /// variant — mirrors how `aura_job::JobError::Storage` is produced.
    #[error("memory storage error: {0}")]
    Storage(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<aura_store::StorageError> for MemoryError {
    fn from(e: aura_store::StorageError) -> Self {
        match e {
            aura_store::StorageError::NotFound(s) => MemoryError::NotFound(s),
            aura_store::StorageError::Internal(e) => MemoryError::Internal(e),
            other => MemoryError::Storage(other.to_string()),
        }
    }
}
