use thiserror::Error;

#[derive(Debug, Error)]
pub enum ContextError {
    #[error("context compression error: {0}")]
    Compression(String),

    /// The summarizer call was cancelled — the turn is unwinding (`/stop`,
    /// a preempt). Distinct from [`ContextError::Compression`] because the
    /// compressor must NOT apply its truncate fallback here: nothing else
    /// will be sent to the model on this turn, so destroying the middle of
    /// the transcript would be pure loss.
    #[error("context compaction cancelled: {0}")]
    Cancelled(String),

    #[error("context snapshot error: {0}")]
    Snapshot(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
