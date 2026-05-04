use aura_model::SessionId;
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

    /// `SessionStore::soft_delete` rejected the request because the
    /// target session has live forks pointing at it. The caller must
    /// delete the listed forks first (or accept the error) — there is
    /// no materialize-on-delete escape hatch.
    #[error("session has {} live fork(s); delete forks first", .fork_session_ids.len())]
    HasLiveForks { fork_session_ids: Vec<SessionId> },

    /// Generic wrapper for unexpected lower-layer errors (e.g. libsql
    /// driver failures that don't map cleanly onto a richer variant).
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
