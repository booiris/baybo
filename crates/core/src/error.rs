use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuraError {
    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid state transition: {0}")]
    InvalidTransition(String),

    #[error("limit exceeded: {0}")]
    LimitExceeded(String),

    #[error("security error: {0}")]
    Security(String),

    #[error("timeout: {0}")]
    Timeout(String),
}
