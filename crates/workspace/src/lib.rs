pub mod identity;

use std::path::PathBuf;

pub use identity::{IdentityFiles, IdentityKind};

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

    /// Atomically write one identity document to the workspace root.
    ///
    /// Overwrites the previous copy. Returns the absolute path that was
    /// written. The new content is not picked up by any already-loaded
    /// `Soul` / agent context until the process is restarted.
    pub async fn write_identity_file(
        &self,
        kind: IdentityKind,
        content: &str,
    ) -> anyhow::Result<PathBuf> {
        identity::write_identity_file(&self.root, kind, content).await
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
    }
}
