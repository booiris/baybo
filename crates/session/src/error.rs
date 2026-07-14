use thiserror::Error;

/// Errors surfaced by the session subsystem — covers both
/// [`SessionManager`](crate::SessionManager) orchestration and the
/// underlying [`SessionStore`](crate::SessionStore) /
/// [`SessionSummaryStore`](crate::SessionSummaryStore) persistence
/// boundary.
///
/// Storage-layer failures collapse into [`SessionError::Storage`] —
/// the sqlite backend's structured `StorageError` is stringified at
/// the boundary so callers never need to depend on `baybo-storage`'s
/// error type.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session not found: {0}")]
    NotFound(String),

    #[error("session storage error: {0}")]
    Storage(String),

    #[error("session state error: {0}")]
    InvalidState(String),

    /// A folder operation violated an invariant (name too long, depth cap,
    /// cycle). Maps to a 400 at the gateway, distinct from `NotFound`.
    #[error("invalid folder operation: {0}")]
    InvalidFolderOp(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// Bridge the sqlite-backed store error to the public `SessionError`.
/// Stringifies generic failures into [`SessionError::Storage`].
impl From<baybo_store::StorageError> for SessionError {
    fn from(e: baybo_store::StorageError) -> Self {
        match e {
            baybo_store::StorageError::NotFound(s) => SessionError::NotFound(s),
            baybo_store::StorageError::Storage(s) => SessionError::Storage(s),
            baybo_store::StorageError::Conflict(s) => SessionError::Storage(s),
            other @ baybo_store::StorageError::TooLarge { .. } => {
                SessionError::Storage(other.to_string())
            }
            baybo_store::StorageError::Internal(e) => SessionError::Internal(e),
        }
    }
}
