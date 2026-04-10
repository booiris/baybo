use thiserror::Error;

#[derive(Debug, Error)]
pub enum HookError {
    #[error("hook execution failed: {0}")]
    Execution(String),

    #[error("hook aborted: {0}")]
    Aborted(String),

    #[error("hook blocked: {0}")]
    Blocked(String),

    #[error("hook timed out after {timeout_secs}s")]
    Timeout { timeout_secs: u64 },

    #[error("invalid matcher pattern: {0}")]
    InvalidMatcher(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
