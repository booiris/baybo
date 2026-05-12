use thiserror::Error;

use crate::kind::ChannelKind;

#[derive(Debug, Error)]
pub enum ChannelError {
    #[error("channel send error: {0}")]
    Send(String),

    #[error("channel configuration error: {0}")]
    Config(String),

    /// Couldn't reach the channel transport at all (e.g. no gateway
    /// process listening on the UDS). Distinguished from [`Config`] so
    /// callers can tell "nothing on the other end" from "handshake
    /// failed on a live endpoint" — the auto-spawn gateway path in the
    /// TUI only fires on this variant.
    #[error("channel endpoint not reachable: {0}")]
    NotReachable(String),

    #[error("channel {0} already installed")]
    DuplicateChannel(String),

    #[error("connection {0} not found on channel")]
    ConnectionNotFound(String),

    #[error("channel {channel_type} has kind {actual:?}, operation requires kind {expected:?}")]
    WrongKind {
        channel_type: String,
        expected: ChannelKind,
        actual: ChannelKind,
    },
}
