use thiserror::Error;

#[derive(Debug, Error)]
pub enum ContextError {
    #[error("context compression error: {0}")]
    Compression(String),

    #[error("context snapshot error: {0}")]
    Snapshot(String),

    /// The summarization LLM returned empty / whitespace-only content.
    /// `Summarize::compress` treats this the same as a transport error
    /// and falls back to truncation.
    #[error("summarization returned empty content")]
    EmptySummary,

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
