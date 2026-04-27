use std::path::PathBuf;

use crate::identity::{self, IdentityFiles};
use crate::paths::{GITIGNORE_CONTENTS, IdentityKind, WorkspacePaths};

/// Manages the workspace root directory and its identity/configuration files.
pub struct WorkspaceManager {
    pub root: PathBuf,
}

impl WorkspaceManager {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Materialise the workspace skeleton: create `profile/`, `skills/`,
    /// `state/`, `work/`, `logs/`, and write the allowlist `.gitignore` if
    /// it does not already exist. Idempotent — safe to call on every boot.
    ///
    /// Existing `.gitignore` files are left untouched so users can hand-edit
    /// the allowlist (e.g. add their own subdirectory).
    pub async fn ensure_layout(&self) -> anyhow::Result<()> {
        let paths = WorkspacePaths::new(self.root.clone());
        for dir in [
            paths.profile_dir(),
            paths.skills_dir(),
            paths.state_dir(),
            paths.work_dir(),
            paths.logs_dir(),
        ] {
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|e| anyhow::anyhow!("create workspace dir {}: {e}", dir.display()))?;
        }

        let gitignore = paths.gitignore_file();
        if !gitignore.exists() {
            tokio::fs::write(&gitignore, GITIGNORE_CONTENTS)
                .await
                .map_err(|e| anyhow::anyhow!("write {}: {e}", gitignore.display()))?;
        }
        Ok(())
    }

    /// Loads all identity files from the workspace `profile/` directory.
    /// Missing files are represented as `None` rather than causing errors.
    pub async fn load_identity_files(&self) -> anyhow::Result<IdentityFiles> {
        identity::load_identity_files(&self.root).await
    }

    /// Atomically write one identity document to the workspace `profile/`
    /// directory.
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

    #[tokio::test]
    async fn ensure_layout_creates_dirs_and_gitignore() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test_tmp")
            .join("workspace_layout_test");
        let _ = tokio::fs::remove_dir_all(&dir).await;

        let mgr = WorkspaceManager::new(dir.clone());
        mgr.ensure_layout().await.expect("layout");
        let paths = WorkspacePaths::new(dir.clone());

        for d in [
            paths.profile_dir(),
            paths.skills_dir(),
            paths.state_dir(),
            paths.work_dir(),
            paths.logs_dir(),
        ] {
            assert!(d.exists(), "missing dir {}", d.display());
        }
        let gitignore = tokio::fs::read_to_string(paths.gitignore_file())
            .await
            .unwrap();
        assert!(gitignore.contains("!/profile/"));
        assert!(gitignore.contains("!/skills/"));

        // Idempotent: a hand-edited .gitignore must not be overwritten.
        tokio::fs::write(paths.gitignore_file(), "# hand-edited\n")
            .await
            .unwrap();
        mgr.ensure_layout().await.expect("layout reapply");
        let after = tokio::fs::read_to_string(paths.gitignore_file())
            .await
            .unwrap();
        assert_eq!(after, "# hand-edited\n");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
