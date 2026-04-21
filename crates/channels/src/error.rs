use thiserror::Error;

#[derive(Debug, Error)]
pub enum ChannelError {
    #[error("channel send error: {0}")]
    Send(String),

    #[error("channel receive error: {0}")]
    Receive(String),

    #[error("channel not started")]
    NotStarted,

    #[error("channel already started")]
    AlreadyStarted,

    #[error("channel configuration error: {0}")]
    Config(String),

    /// Couldn't reach the channel transport at all (e.g. no gateway
    /// process listening on the UDS). Distinguished from [`Config`] so
    /// callers can tell "nothing on the other end" from "handshake
    /// failed on a live endpoint" — the auto-spawn gateway path in the
    /// TUI only fires on this variant.
    #[error("channel endpoint not reachable: {0}")]
    NotReachable(String),

    #[error("channel {0} already registered")]
    DuplicateChannel(String),

    #[error("session {0} already has an attached client")]
    DuplicateSessionClient(String),

    #[error("channel {0} not found")]
    NotFound(String),

    #[error("session client {0} not found")]
    SessionClientNotFound(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
