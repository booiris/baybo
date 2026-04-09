use thiserror::Error;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool not found: {0}")]
    NotFound(String),

    #[error("tool execution error: {0}")]
    Execution(String),

    #[error("tool timeout: {0}")]
    Timeout(String),

    #[error("WASM error: {0}")]
    Wasm(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
