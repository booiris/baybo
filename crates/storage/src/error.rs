use aura_model::SessionId;
use aura_session::SessionError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("storage error: {0}")]
    Storage(String),

    #[error("not found: {0}")]
    NotFound(String),

    /// Streaming put exceeded the caller-supplied byte cap. Surfaces as
    /// HTTP 413 at the gateway boundary; internal callers can also use
    /// it to short-circuit oversized writes without buffering the rest.
    #[error("payload too large: {actual} bytes exceeds limit of {limit}")]
    TooLarge { limit: u64, actual: u64 },

    /// `SessionStore::delete` rejected the request because the target
    /// session has forks pointing at it. The caller must delete the
    /// listed forks first (or accept the error) — there is no
    /// materialize-on-delete escape hatch.
    #[error("session has {} live fork(s); delete forks first", .fork_session_ids.len())]
    HasLiveForks { fork_session_ids: Vec<SessionId> },

    /// Generic wrapper for unexpected lower-layer errors (e.g. libsql
    /// driver failures that don't map cleanly onto a richer variant).
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// Bridge libsql-backed session/summary store errors to the public
/// `SessionError` returned by the trait. Stringifies generic libsql
/// failures into `SessionError::Storage`; preserves `HasLiveForks`
/// structurally so the CLI delete path can render the fork ids back.
impl From<StorageError> for SessionError {
    fn from(e: StorageError) -> Self {
        match e {
            StorageError::HasLiveForks { fork_session_ids } => {
                SessionError::HasLiveForks { fork_session_ids }
            }
            StorageError::NotFound(s) => SessionError::NotFound(s),
            StorageError::Storage(s) => SessionError::Storage(s),
            other @ StorageError::TooLarge { .. } => SessionError::Storage(other.to_string()),
            StorageError::Internal(e) => SessionError::Internal(e),
        }
    }
}
