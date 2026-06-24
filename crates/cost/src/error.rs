use thiserror::Error;

#[derive(Debug, Error)]
pub enum CostError {
    #[error("cost storage error: {0}")]
    Storage(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type CostResult<T> = std::result::Result<T, CostError>;

impl From<baybo_store::StorageError> for CostError {
    fn from(e: baybo_store::StorageError) -> Self {
        match e {
            baybo_store::StorageError::Internal(e) => CostError::Internal(e),
            other => CostError::Storage(other.to_string()),
        }
    }
}
