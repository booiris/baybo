use thiserror::Error;

#[derive(Debug, Error)]
pub enum HookError {
    #[error("hook execution failed: {0}")]
    Execution(String),

    #[error("hook aborted: {0}")]
    Aborted(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
