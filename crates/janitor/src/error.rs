use aura_storage::StorageError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum JanitorError {
    #[error("filesystem sweep at {path}: {source}")]
    Filesystem {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("blob store: {0}")]
    Blob(#[from] StorageError),
}

impl JanitorError {
    pub(crate) fn fs(path: impl std::fmt::Display, source: std::io::Error) -> Self {
        Self::Filesystem {
            path: path.to_string(),
            source,
        }
    }
}
