pub mod heartbeat;
pub mod identity;

use std::path::PathBuf;

pub use heartbeat::{HeartbeatSpec, RoutineSchedule, RoutineSpec};
pub use identity::IdentityFiles;

/// Manages the workspace root directory and its identity/configuration files.
pub struct WorkspaceManager {
    pub root: PathBuf,
}

impl WorkspaceManager {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Loads all identity files from the workspace root.
    /// Missing files are represented as `None` rather than causing errors.
    pub async fn load_identity_files(&self) -> aura_core::Result<IdentityFiles> {
        identity::load_identity_files(&self.root).await
    }

    /// Parses the HEARTBEAT.md file into a structured spec.
    /// Returns `Ok(None)` if the file does not exist.
    pub async fn load_heartbeat_spec(&self) -> aura_core::Result<Option<HeartbeatSpec>> {
        let files = self.load_identity_files().await?;
        match files.heartbeat {
            Some(content) => heartbeat::parse_heartbeat(&content).map(Some),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_missing_workspace_dir() {
        let mgr = WorkspaceManager::new(PathBuf::from("/nonexistent/path"));
        let files = mgr.load_identity_files().await.unwrap();
        assert!(files.agents.is_none());
        assert!(files.soul.is_none());
        assert!(files.user.is_none());
        assert!(files.identity.is_none());
        assert!(files.heartbeat.is_none());
    }

    #[tokio::test]
    async fn test_missing_heartbeat() {
        let mgr = WorkspaceManager::new(PathBuf::from("/nonexistent/path"));
        let spec = mgr.load_heartbeat_spec().await.unwrap();
        assert!(spec.is_none());
    }
}
