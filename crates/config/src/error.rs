use thiserror::Error;

/// Errors produced while loading, parsing, or validating Aura configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file '{path}': {reason}")]
    FileRead { path: String, reason: String },

    #[error("failed to parse config: {0}")]
    Parse(String),

    #[error("config validation failed:\n{}", format_errors(.0))]
    Validation(Vec<ValidationError>),

    #[error("unsupported variant for {ty}: '{variant}'")]
    UnsupportedVariant { ty: String, variant: String },

    #[error("invalid config path '{path}': {reason}")]
    InvalidPath { path: String, reason: String },

    #[error("failed to write config file '{path}': {reason}")]
    FileWrite { path: String, reason: String },

    #[error(
        "config section '{section}' is not hot-updatable; that change requires a gateway restart"
    )]
    NotHotReloadable { section: String },
}

/// A single validation failure, identifying the offending field and the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    pub field: String,
    pub message: String,
}

impl ValidationError {
    pub(crate) fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "  {}: {}", self.field, self.message)
    }
}

fn format_errors(errors: &[ValidationError]) -> String {
    errors
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

pub type Result<T> = std::result::Result<T, ConfigError>;
