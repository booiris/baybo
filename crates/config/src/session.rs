use serde::{Deserialize, Serialize};

/// Session management configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SessionConfig {
    /// Session idle timeout in minutes. Sessions are expired after this duration
    /// of inactivity.
    pub timeout_minutes: u64,
    /// How often (in minutes) the cleanup task runs. `0` disables cleanup.
    pub cleanup_interval_minutes: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            timeout_minutes: 30,
            cleanup_interval_minutes: 0,
        }
    }
}
