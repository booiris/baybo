use thiserror::Error;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool not found: {0}")]
    NotFound(String),

    #[error("invalid parameters: {0}")]
    InvalidParams(String),

    #[error("tool execution error: {0}")]
    Execution(String),

    #[error("tool timeout: {0}")]
    Timeout(String),

    #[error("tool not implemented: {0}")]
    NotImplemented(String),

    /// The user denied the approval request for this call.
    #[error("tool '{tool}' denied by user: {reason}")]
    Denied { tool: String, reason: String },

    #[error("mcp error: {0}")]
    Mcp(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
