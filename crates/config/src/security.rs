use aura_workspace::paths::{ENCRYPTION_KEY_FILE, KEY_DIR, default_workspace_root};
use serde::{Deserialize, Serialize};

/// Security-related configuration: encryption key location and leak detection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SecurityConfig {
    /// Absolute path to a file containing a 32-byte hex-encoded encryption
    /// key. Required: validation refuses to load a config without it.
    /// `aura setup` mints one under `<workspace>/.key/encryption.key`
    /// (mode 0600) on first run; `Default` points at the same location
    /// derived from `default_workspace_root()` so a fresh
    /// `AuraConfig::default()` still passes validation as long as the
    /// file has been minted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encryption_key_file: Option<String>,
    /// Whether to run the leak detector on incoming/outgoing messages.
    pub leak_detection_enabled: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        let default_key = default_workspace_root()
            .join(KEY_DIR)
            .join(ENCRYPTION_KEY_FILE)
            .to_string_lossy()
            .into_owned();
        Self {
            encryption_key_file: Some(default_key),
            leak_detection_enabled: true,
        }
    }
}
