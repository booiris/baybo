use thiserror::Error;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("invalid mcp configuration: {0}")]
    InvalidConfig(String),

    #[error("failed to read .mcp.json at {path}: {reason}")]
    FileRead { path: String, reason: String },

    #[error("failed to write .mcp.json at {path}: {reason}")]
    FileWrite { path: String, reason: String },

    #[error("failed to parse .mcp.json: {0}")]
    Parse(String),

    #[error("server '{0}' already exists")]
    DuplicateName(String),

    #[error("server '{0}' not found")]
    NotFound(String),

    #[error("transport error: {0}")]
    Transport(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("oauth error: {0}")]
    OAuth(String),

    #[error("connection error: {0}")]
    Connection(String),
}

pub type McpResult<T> = std::result::Result<T, McpError>;
