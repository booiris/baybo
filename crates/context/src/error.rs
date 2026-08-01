use thiserror::Error;

#[derive(Debug, Error)]
pub enum ContextError {
    #[error("context compression error: {0}")]
    Compression(String),

    /// The summariser call was aborted because the turn was cancelled.
    /// Distinct from [`ContextError::Compression`] because a cancel is not a
    /// failure: nothing went wrong, so the user is not told about it, and the
    /// next turn re-runs the compaction from scratch.
    #[error("context compaction cancelled: {0}")]
    Cancelled(String),

    #[error("context snapshot error: {0}")]
    Snapshot(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
