use thiserror::Error;

#[derive(Debug, Error)]
pub enum CostError {
    #[error("cost storage error: {0}")]
    Storage(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
