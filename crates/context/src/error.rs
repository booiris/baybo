use thiserror::Error;

#[derive(Debug, Error)]
pub enum ContextError {
    #[error("context compression error: {0}")]
    Compression(String),

    /// A background-summary pass ran to completion but landed no `Edit`,
    /// so `summary.md` still covers only what the previous pass covered.
    /// Distinct from [`ContextError::Compression`] because the caller
    /// backs off on it: retrying immediately would re-send the whole
    /// transcript to a model that just declined to write anything.
    #[error("background summary landed no edits: {0}")]
    UnproductiveSummary(String),

    #[error("context snapshot error: {0}")]
    Snapshot(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
