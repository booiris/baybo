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

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
