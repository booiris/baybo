pub mod heartbeat;
pub mod identity;

use std::path::PathBuf;

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
    pub async fn load_identity_files(&self) -> anyhow::Result<IdentityFiles> {
        identity::load_identity_files(&self.root).await
    }
}

#[cfg(test)]
impl WorkspaceManager {
    async fn load_heartbeat_spec(&self) -> anyhow::Result<Option<heartbeat::HeartbeatSpec>> {
        let identity = self.load_identity_files().await?;
        match identity.heartbeat {
            Some(content) => {
                let spec = heartbeat::parse_heartbeat(&content)?;
                Ok(Some(spec))
            }
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
