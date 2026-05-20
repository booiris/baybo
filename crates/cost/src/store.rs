use thiserror::Error;

#[derive(Debug, Error)]
pub enum CostError {
    #[error("cost storage error: {0}")]
    Storage(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type CostResult<T> = std::result::Result<T, CostError>;

impl From<aura_store::StorageError> for CostError {
    fn from(e: aura_store::StorageError) -> Self {
        match e {
            aura_store::StorageError::Internal(e) => CostError::Internal(e),
            other => CostError::Storage(other.to_string()),
        }
    }
}
