use serde::{Deserialize, Serialize};

/// Project-root configuration.
///
/// `path` is the single workspace / project root. All persistent data
/// (libsql storage, identity files, etc.) is composed relative to this
/// path; individual subsystems do not expose their own path knobs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WorkspaceConfig {
    pub path: String,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            path: ".".to_string(),
        }
    }
}
