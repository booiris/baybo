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

    /// Materialise the workspace skeleton: create `config/`, `profile/`,
    /// `skills/`, `.key/`, `state/`, `work/`, `logs/`, and initialise a
    /// standalone git repo inside each of `config/`, `profile/`, and
    /// `skills/` if it isn't one already. Idempotent — safe to call on
    /// every boot.
    pub async fn ensure_layout(&self) -> anyhow::Result<()> {
        let paths = WorkspacePaths::new(self.root.clone());
        for dir in [
            paths.config_dir(),
            paths.profile_dir(),
            paths.skills_dir(),
            paths.key_dir(),
            paths.state_dir(),
            // Per-session writable artifacts (currently `summary.md` for
            // the async summary-refresh design). Per-session subdirs are
            // created lazily by the BackgroundCompressionRunner; this just guarantees
            // the parent dir exists so the lazy create can land.
            paths.state_sessions_dir(),
            paths.work_dir(),
            paths.logs_dir(),
        ] {
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|e| anyhow::anyhow!("create workspace dir {}: {e}", dir.display()))?;
        }

        for dir in [paths.config_dir(), paths.profile_dir(), paths.skills_dir()] {
            ensure_git_repo(&dir).await?;
        }
        Ok(())
    }

    /// Seed any missing identity markdown file under `profile/` with its
    /// default template. Existing files are left untouched, so an
    /// operator who deletes a file (or replaces its contents) is never
    /// silently overridden. Intended to run once at setup time —
    /// `aura-setup::bootstrap` invokes it after `ensure_layout` — rather
    /// than on every boot, so a deliberately-deleted identity file
    /// stays deleted.
    ///
    /// Assumes `profile/` already exists (i.e. `ensure_layout` has
    /// run).
    pub async fn seed_default_identity_files(&self) -> anyhow::Result<()> {
        let paths = WorkspacePaths::new(self.root.clone());
        for kind in IdentityKind::all() {
            let target = paths.identity_file(kind);
            let exists = tokio::fs::try_exists(&target)
                .await
                .map_err(|e| anyhow::anyhow!("stat {}: {e}", target.display()))?;
            if exists {
                continue;
            }
            tokio::fs::write(&target, kind.default_content())
                .await
                .map_err(|e| {
                    anyhow::anyhow!("seed default identity file {}: {e}", target.display())
                })?;
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
            paths.config_dir(),
            paths.profile_dir(),
            paths.skills_dir(),
            paths.key_dir(),
            paths.state_dir(),
            paths.work_dir(),
            paths.logs_dir(),
        ] {
            assert!(d.exists(), "missing dir {}", d.display());
        }
        // No workspace-root .gitignore should exist anymore.
        assert!(!dir.join(".gitignore").exists());
        // Each of config/, profile/, and skills/ is its own git repo.
        assert!(paths.config_dir().join(".git").is_dir());
        assert!(paths.profile_dir().join(".git").is_dir());
        assert!(paths.skills_dir().join(".git").is_dir());
        // .key/ is NOT a git repo — encryption key must never be tracked.
        assert!(!paths.key_dir().join(".git").exists());

        // Idempotent: a re-apply must not re-init or fail.
        mgr.ensure_layout().await.expect("layout reapply");
        assert!(paths.config_dir().join(".git").is_dir());
        assert!(paths.profile_dir().join(".git").is_dir());
        assert!(paths.skills_dir().join(".git").is_dir());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn ensure_layout_does_not_seed_identity_files() {
        // `ensure_layout` is the dir-skeleton hook; identity defaults
        // belong to setup so a deleted file isn't resurrected on every
        // boot. Guard the contract here.
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test_tmp")
            .join("workspace_layout_no_seed_test");
        let _ = tokio::fs::remove_dir_all(&dir).await;

        let mgr = WorkspaceManager::new(dir.clone());
        mgr.ensure_layout().await.expect("layout");

        let loaded = mgr.load_identity_files().await.expect("load");
        assert!(loaded.soul.is_none());
        assert!(loaded.user.is_none());
        assert!(loaded.identity.is_none());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn seed_default_identity_files_writes_each_default() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test_tmp")
            .join("workspace_seed_defaults_test");
        let _ = tokio::fs::remove_dir_all(&dir).await;

        let mgr = WorkspaceManager::new(dir.clone());
        mgr.ensure_layout().await.expect("layout");
        mgr.seed_default_identity_files().await.expect("seed");

        let loaded = mgr.load_identity_files().await.expect("load");
        assert_eq!(
            loaded.soul.as_deref(),
            Some(IdentityKind::Soul.default_content())
        );
        assert_eq!(
            loaded.user.as_deref(),
            Some(IdentityKind::User.default_content())
        );
        assert_eq!(
            loaded.identity.as_deref(),
            Some(IdentityKind::Identity.default_content())
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn seed_default_identity_files_preserves_user_edits() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test_tmp")
            .join("workspace_preserve_edits_test");
        let _ = tokio::fs::remove_dir_all(&dir).await;

        let mgr = WorkspaceManager::new(dir.clone());
        mgr.ensure_layout().await.expect("first layout");
        mgr.seed_default_identity_files().await.expect("first seed");

        const CUSTOM: &str = "# Soul\n\nHand-edited by the operator.\n";
        mgr.write_identity_file(IdentityKind::Soul, CUSTOM)
            .await
            .expect("operator edit");

        // A second seed must keep the operator edit intact and leave
        // the rest of the defaults alone.
        mgr.seed_default_identity_files().await.expect("re-seed");
        let loaded = mgr.load_identity_files().await.expect("load");
        assert_eq!(loaded.soul.as_deref(), Some(CUSTOM));
        assert_eq!(
            loaded.identity.as_deref(),
            Some(IdentityKind::Identity.default_content())
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
