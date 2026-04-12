use serde::{Deserialize, Serialize};

/// Security-related configuration: encryption key location and leak detection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SecurityConfig {
    /// Path to a file containing a 32-byte hex-encoded encryption key.
    /// When `None`, the consumer falls back to the environment variable named
    /// by `encryption_key_env`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encryption_key_file: Option<String>,
    /// Environment variable to consult for the encryption key.
    pub encryption_key_env: String,
    /// Whether to run the leak detector on incoming/outgoing messages.
    pub leak_detection_enabled: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            encryption_key_file: None,
            encryption_key_env: "AURA_ENCRYPTION_KEY".to_string(),
            leak_detection_enabled: true,
        }
    }
}
