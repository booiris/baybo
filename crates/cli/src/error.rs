use thiserror::Error;

/// Errors produced by the CLI layer.
///
/// Specific manager errors are wrapped via `.to_string()` rather than `#[from]`
/// so that `baybo-cli` does not take a hard dependency on every domain crate's
/// error enum.
#[derive(Debug, Error)]
pub enum CliError {
    #[error("failed to parse command: {0}")]
    Parse(String),

    #[error("unknown command: {0}")]
    UnknownCommand(String),

    #[error("command requires --yes confirmation in slash mode: {0}")]
    ConfirmationRequired(String),

    #[error("command '{0}' is not available inside a chat session; run it from the shell")]
    AgentSendForbiddenInSlash(String),

    #[error("command is not available in slash mode: {0}")]
    NotAvailableInSlash(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(String),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("manager error: {0}")]
    Manager(String),
}

impl From<std::io::Error> for CliError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err.to_string())
    }
}

impl From<serde_json::Error> for CliError {
    fn from(err: serde_json::Error) -> Self {
        Self::Serialization(err.to_string())
    }
}

impl From<baybo_config::ConfigError> for CliError {
    fn from(err: baybo_config::ConfigError) -> Self {
        Self::Config(err.to_string())
    }
}

impl From<baybo_setup::SetupError> for CliError {
    fn from(err: baybo_setup::SetupError) -> Self {
        use baybo_setup::SetupError as S;
        match err {
            S::NotATerminal | S::Cancelled | S::Prompt(_) => Self::Config(err.to_string()),
            S::Io { .. } => Self::Io(err.to_string()),
            S::Config(_) | S::Vault(_) | S::Storage(_) | S::Browser(_) | S::Gateway(_) => {
                Self::Config(err.to_string())
            }
            S::Llm(_) | S::Channel(_) => Self::Manager(err.to_string()),
        }
    }
}

pub type Result<T> = std::result::Result<T, CliError>;
