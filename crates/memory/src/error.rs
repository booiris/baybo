use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemoryError {
    /// The memory backend (vector store, external API, …) failed. The
    /// implementation stringifies its own lower-layer error into this variant —
    /// the core keeps memory storage opaque, so it never names a concrete
    /// backend error type.
    #[error("memory backend error: {0}")]
    Backend(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
