use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("LLM provider error: {0}")]
    Provider(String),

    #[error("LLM configuration error: {0}")]
    Config(String),

    #[error("model not found: {0}")]
    ModelNotFound(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
