use aura_model::SessionId;
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
/// error type. The one exception is [`SessionError::HasLiveForks`],
/// which preserves the fork id list because the `aura session delete`
/// CLI surface and the gateway delete handler render those ids back
/// to the operator.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session not found: {0}")]
    NotFound(String),

    #[error("session storage error: {0}")]
    Storage(String),

    #[error("session state error: {0}")]
    InvalidState(String),

    /// `SessionStore::delete` rejected the request because the target
    /// session has user-fork descendants. The caller must delete the
    /// listed forks first (or accept the error) — there is no
    /// materialize-on-delete escape hatch.
    #[error("session has {} live fork(s); delete forks first", .fork_session_ids.len())]
    HasLiveForks { fork_session_ids: Vec<SessionId> },

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
