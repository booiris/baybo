use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, SetupError>;

/// All recoverable failures the wizard can hit. Contained per the
/// project rule — every crate owns its error enum, no shared root.
#[derive(Debug, Error)]
pub enum SetupError {
    #[error("interactive setup requires a TTY on both stdin and stderr")]
    NotATerminal,

    #[error("setup was cancelled by the user")]
    Cancelled,

    #[error("filesystem error at {path}: {reason}")]
    Io { path: PathBuf, reason: String },

    #[error("config error: {0}")]
    Config(String),

    #[error("vault error: {0}")]
    Vault(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("llm step: {0}")]
    Llm(String),

    #[error("channel step: {0}")]
    Channel(String),

    #[error("browser step: {0}")]
    Browser(String),

    #[error("gateway launch: {0}")]
    Gateway(String),

    #[error("prompt error: {0}")]
    Prompt(String),
}

impl SetupError {
    pub(crate) fn io(path: impl Into<PathBuf>, reason: impl Into<String>) -> Self {
        Self::Io {
            path: path.into(),
            reason: reason.into(),
        }
    }
}
