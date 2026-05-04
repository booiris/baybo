use std::path::{Path, PathBuf};

use crate::identity::{self, IdentityFiles};
use crate::paths::{IdentityKind, WorkspacePaths};

/// Manages the workspace root directory and its identity/configuration files.
pub struct WorkspaceManager {
    pub root: PathBuf,
}

impl WorkspaceManager {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Materialise the workspace skeleton: create `profile/`, `skills/`,
    /// `state/`, `work/`, `logs/`, and initialise a standalone git repo
    /// inside each of `profile/` and `skills/` if it isn't one already.
    /// Idempotent — safe to call on every boot.
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

        for dir in [paths.profile_dir(), paths.skills_dir()] {
            ensure_git_repo(&dir).await?;
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

/// Initialise a standalone git repository inside `dir` if one isn't
/// already there. Idempotent — a no-op when `<dir>/.git` exists.
async fn ensure_git_repo(dir: &Path) -> anyhow::Result<()> {
    if dir.join(".git").exists() {
        return Ok(());
    }
    let status = tokio::process::Command::new("git")
        .arg("init")
        .arg("--quiet")
        .arg(dir)
        .status()
        .await
        .map_err(|e| anyhow::anyhow!("spawn `git init {}`: {e}", dir.display()))?;
    if !status.success() {
        return Err(anyhow::anyhow!(
            "`git init {}` exited with {status}",
            dir.display()
        ));
    }
    Ok(())
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
    async fn ensure_layout_creates_dirs_and_local_git_repos() {
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
        // No workspace-root .gitignore should exist anymore.
        assert!(!dir.join(".gitignore").exists());
        // Each of profile/ and skills/ is its own git repo.
        assert!(paths.profile_dir().join(".git").is_dir());
        assert!(paths.skills_dir().join(".git").is_dir());

        // Idempotent: a re-apply must not re-init or fail.
        mgr.ensure_layout().await.expect("layout reapply");
        assert!(paths.profile_dir().join(".git").is_dir());
        assert!(paths.skills_dir().join(".git").is_dir());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
