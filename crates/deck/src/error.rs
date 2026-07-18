use thiserror::Error;

#[derive(Debug, Error)]
pub enum DeckError {
    /// The bundle failed static validation (malformed manifest/spec,
    /// size caps, missing files). The message is agent-facing: it is
    /// returned in the install tool's result so the agent can fix the
    /// bundle in the same turn.
    #[error("invalid bundle: {0}")]
    InvalidBundle(String),

    /// The dry-run gate refused the card (boot failure, refresh op
    /// error/timeout, off-schema snapshot).
    #[error("dry-run failed: {0}")]
    DryRun(String),

    /// A gateway-crossing op call was refused by the card's admission
    /// contract (unknown op, off-schema params).
    #[error("op rejected: {0}")]
    OpRejected(String),

    #[error("card not found: {0}")]
    NotFound(String),

    /// The deck is at its card cap; installs and restores are refused.
    #[error("deck is full ({0} cards)")]
    DeckFull(usize),

    /// The card's service is not running (disabled, quarantined, or
    /// mid-restart) and the call cannot be served.
    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),

    #[error("storage: {0}")]
    Storage(#[from] baybo_store::StorageError),

    #[error("sandbox: {0}")]
    Sandbox(#[from] baybo_sandbox::SandboxError),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, DeckError>;
