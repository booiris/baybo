use aura_workspace::paths::default_workspace_root;
use serde::{Deserialize, Serialize};

/// Project-root configuration.
///
/// `path` is the single workspace / project root — the directory that holds
/// all persistent aura data (libsql storage, lock file, identity files).
/// Individual subsystems do not expose their own path knobs; they compose
/// directly under `path`.
///
/// Defaults: `~/.aura` in release builds, `./.aura` in debug builds. The
/// debug default keeps `cargo run` self-contained inside the project
/// checkout rather than polluting the real user home.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WorkspaceConfig {
    pub path: String,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            path: default_workspace_root().to_string_lossy().into_owned(),
        }
    }
}
