use thiserror::Error;

/// Errors surfaced by the session subsystem — covers both
/// [`SessionManager`](crate::SessionManager) orchestration and the
/// underlying [`SessionStore`](crate::SessionStore) /
/// [`SessionSummaryStore`](crate::SessionSummaryStore) persistence
/// boundary.
///
/// Storage-layer failures collapse into [`SessionError::Storage`] —
/// the libsql backend's structured `StorageError` is stringified at
/// the boundary so callers never need to depend on `aura-storage`'s
/// error type.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session not found: {0}")]
    NotFound(String),

    #[error("session storage error: {0}")]
    Storage(String),

    #[error("session state error: {0}")]
    InvalidState(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// Bridge the libsql-backed store error to the public `SessionError`.
/// Stringifies generic failures into [`SessionError::Storage`].
impl From<aura_store::StorageError> for SessionError {
    fn from(e: aura_store::StorageError) -> Self {
        match e {
            aura_store::StorageError::NotFound(s) => SessionError::NotFound(s),
            aura_store::StorageError::Storage(s) => SessionError::Storage(s),
            aura_store::StorageError::Conflict(s) => SessionError::Storage(s),
            other @ aura_store::StorageError::TooLarge { .. } => {
                SessionError::Storage(other.to_string())
            }
            aura_store::StorageError::Internal(e) => SessionError::Internal(e),
        }
    }
}
