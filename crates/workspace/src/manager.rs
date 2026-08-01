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

    /// Materialise the workspace skeleton: create `config/`,
    /// `skills/`, `agents/`, `personas/`, `.key/`, `state/`, `work/`,
    /// `work/tmp/`, `logs/`, and initialise a standalone git repo inside
    /// each of the declarative dirs if it isn't one already. Idempotent —
    /// safe to call on every boot.
    pub async fn ensure_layout(&self) -> anyhow::Result<()> {
        let paths = WorkspacePaths::new(self.root.clone());
        migrate_profile_into_personas(&paths).await?;
        for dir in [
            paths.config_dir(),
            paths.skills_dir(),
            paths.agents_dir(),
            paths.personas_dir(),
            paths.key_dir(),
            paths.state_dir(),
            paths.work_dir(),
            paths.work_tmp_dir(),
            paths.logs_dir(),
        ] {
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|e| anyhow::anyhow!("create workspace dir {}: {e}", dir.display()))?;
        }

        for dir in [
            paths.config_dir(),
            paths.skills_dir(),
            paths.agents_dir(),
            paths.personas_dir(),
        ] {
            ensure_git_repo(&dir).await?;
        }
        Ok(())
    }

    /// Seed any missing identity markdown file for the **built-in** agent
    /// (`personas/baybo/`), plus the shared `personas/USER.md`, with its
    /// default template. Existing files are left untouched, so an
    /// operator who deletes a file (or replaces its contents) is never
    /// silently overridden. Intended to run once at setup time —
    /// `baybo-setup::bootstrap` invokes it after `ensure_layout` — rather
    /// than on every boot, so a deliberately-deleted identity file
    /// stays deleted.
    ///
    /// Assumes `profile/` already exists (i.e. `ensure_layout` has
    /// run).
    pub async fn seed_default_identity_files(&self) -> anyhow::Result<()> {
        let paths = WorkspacePaths::new(self.root.clone());
        let targets = IdentityKind::all()
            .map(|kind| {
                (
                    paths.persona_identity_file(crate::paths::BUILTIN_PERSONA_DIR, kind),
                    kind.default_content(),
                )
            })
            .into_iter()
            .chain([(
                paths.shared_user_file(),
                IdentityKind::User.default_content(),
            )]);
        for (target, default) in targets {
            let exists = tokio::fs::try_exists(&target)
                .await
                .map_err(|e| anyhow::anyhow!("stat {}: {e}", target.display()))?;
            if exists {
                continue;
            }
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| anyhow::anyhow!("create {}: {e}", parent.display()))?;
            }
            tokio::fs::write(&target, default).await.map_err(|e| {
                anyhow::anyhow!("seed default identity file {}: {e}", target.display())
            })?;
        }
        Ok(())
    }

    /// Loads all identity files from the workspace `profile/` directory.
    /// Any missing file is atomically seeded with its default template
    /// (see [`identity::load_identity_files`]), so the returned
    /// [`IdentityFiles`] is always fully populated. The `profile/` dir
    /// itself is created on demand if absent.
    pub async fn load_identity_files(&self) -> anyhow::Result<IdentityFiles> {
        let paths = WorkspacePaths::new(self.root.clone());
        let (soul, identity, user) = identity::builtin_identity_paths(&paths);
        identity::load_identity_files(
            &self.root,
            identity::IdentitySource::new(&soul, IdentityKind::Soul.default_content()),
            identity::IdentitySource::new(&identity, IdentityKind::Identity.default_content()),
            identity::IdentitySource::new(&user, IdentityKind::User.default_content()),
        )
        .await
    }

    /// Atomically write one of the built-in agent's identity documents.
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

/// One-time move of a pre-personas `profile/` into the persona layout.
///
/// `profile/` held the single assistant's three identity files back when
/// there was one assistant. `SOUL.md` and `IDENTITY.md` are that assistant's
/// — they become the built-in's, at `personas/baybo/`. `USER.md` is about
/// the human, so it becomes the *shared* `personas/USER.md` that every agent
/// reads; the built-in starts its own private notes fresh beside the others.
///
/// **Copies, never moves.** `profile/` is left where it is, `.git` history
/// and all, so the five weeks of `Baybo <baybo@local>` audit commits stay
/// readable and nothing is destroyed if this reading of the old layout turns
/// out to be wrong. A file already present at the destination is never
/// overwritten, which is also what makes the pass a no-op on every later
/// boot.
async fn migrate_profile_into_personas(paths: &WorkspacePaths) -> anyhow::Result<()> {
    let legacy = paths.root().join("profile");
    if !tokio::fs::try_exists(&legacy).await.unwrap_or(false) {
        return Ok(());
    }
    let builtin = crate::paths::BUILTIN_PERSONA_DIR;
    let moves = [
        (
            IdentityKind::Soul,
            paths.persona_identity_file(builtin, IdentityKind::Soul),
        ),
        (
            IdentityKind::Identity,
            paths.persona_identity_file(builtin, IdentityKind::Identity),
        ),
        // The human's profile is nobody's private notes.
        (IdentityKind::User, paths.shared_user_file()),
    ];
    for (kind, target) in moves {
        let source = legacy.join(kind.file_name());
        if !tokio::fs::try_exists(&source).await.unwrap_or(false)
            || tokio::fs::try_exists(&target).await.unwrap_or(false)
        {
            continue;
        }
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::copy(&source, &target).await.map_err(|e| {
            anyhow::anyhow!("migrate {} to {}: {e}", source.display(), target.display())
        })?;
        // No `tracing` in this leaf crate; the copy is idempotent and the
        // source stays put, so a silent success is safe to re-observe.
    }
    Ok(())
}

/// Initialise a standalone git repository inside `dir` if one isn't
/// already there. Idempotent — a no-op when `<dir>/.git` exists.
///
/// These repos exist only for optional version history of the agent's
/// skills/profile/agents. The `bench-bash` build skips them (see the no-op
/// below): the workspace is then an ephemeral, single-task container where that
/// history is meaningless, and requiring `git` there only forces a slow
/// per-task `apt-get install git`.
#[cfg(not(feature = "bench-bash"))]
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

/// Bench builds run inside a disposable container with no `git` on PATH and no
/// use for the identity-repo history — so the init is a no-op there.
#[cfg(feature = "bench-bash")]
async fn ensure_git_repo(_dir: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn load_identity_files_errors_when_root_is_unwritable() {
        // Auto-seed needs to create the profile dir; if the root path
        // can't be created (e.g. unwritable parent), the call must
        // surface the error rather than silently returning empty
        // content. `/nonexistent/path` is chosen because no test
        // process should have permission to create it.
        let mgr = WorkspaceManager::new(PathBuf::from("/nonexistent/path"));
        assert!(mgr.load_identity_files().await.is_err());
    }

    /// A workspace from before personas keeps working: its assistant's soul
    /// and self-image become the built-in's, and its notes about the human
    /// become the shared profile every agent reads. Nothing is moved.
    #[tokio::test]
    async fn a_pre_personas_workspace_is_carried_across() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();
        let legacy = dir.join("profile");
        tokio::fs::create_dir_all(&legacy).await.unwrap();
        for (kind, body) in [
            (IdentityKind::Soul, "OLD_SOUL"),
            (IdentityKind::Identity, "OLD_IDENTITY"),
            (IdentityKind::User, "OLD_USER"),
        ] {
            tokio::fs::write(legacy.join(kind.file_name()), body)
                .await
                .unwrap();
        }

        WorkspaceManager::new(dir.clone())
            .ensure_layout()
            .await
            .expect("layout");
        let paths = WorkspacePaths::new(dir.clone());
        let read = async |p: PathBuf| tokio::fs::read_to_string(p).await.unwrap();

        let builtin = crate::paths::BUILTIN_PERSONA_DIR;
        assert_eq!(
            read(paths.persona_identity_file(builtin, IdentityKind::Soul)).await,
            "OLD_SOUL"
        );
        assert_eq!(
            read(paths.persona_identity_file(builtin, IdentityKind::Identity)).await,
            "OLD_IDENTITY"
        );
        // The human's profile becomes the shared one, not the built-in's
        // private notes.
        assert_eq!(read(paths.shared_user_file()).await, "OLD_USER");
        assert!(
            !paths
                .persona_identity_file(builtin, IdentityKind::User)
                .exists(),
            "the built-in starts its private notes fresh"
        );
        // Copied, not moved: the old repo and its audit history stay.
        assert!(legacy.join("SOUL.md").exists());

        // Idempotent, and never overwrites what is already there.
        tokio::fs::write(paths.shared_user_file(), "EDITED")
            .await
            .unwrap();
        WorkspaceManager::new(dir.clone())
            .ensure_layout()
            .await
            .expect("layout again");
        assert_eq!(read(paths.shared_user_file()).await, "EDITED");
    }

    #[tokio::test]
    async fn ensure_layout_creates_dirs_and_local_git_repos() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();

        let mgr = WorkspaceManager::new(dir.clone());
        mgr.ensure_layout().await.expect("layout");
        let paths = WorkspacePaths::new(dir.clone());

        for d in [
            paths.config_dir(),
            paths.skills_dir(),
            paths.agents_dir(),
            paths.personas_dir(),
            paths.key_dir(),
            paths.state_dir(),
            paths.work_dir(),
            paths.work_tmp_dir(),
            paths.logs_dir(),
        ] {
            assert!(d.exists(), "missing dir {}", d.display());
        }
        // No workspace-root .gitignore should exist anymore.
        assert!(!dir.join(".gitignore").exists());
        // Each of config/, profile/, and skills/ is its own git repo.
        assert!(paths.config_dir().join(".git").is_dir());
        assert!(paths.skills_dir().join(".git").is_dir());
        assert!(paths.agents_dir().join(".git").is_dir());
        assert!(paths.personas_dir().join(".git").is_dir());
        // .key/ is NOT a git repo — encryption key must never be tracked.
        assert!(!paths.key_dir().join(".git").exists());

        // Idempotent: a re-apply must not re-init or fail.
        mgr.ensure_layout().await.expect("layout reapply");
        assert!(paths.config_dir().join(".git").is_dir());
        assert!(paths.skills_dir().join(".git").is_dir());
    }

    #[tokio::test]
    async fn ensure_layout_does_not_seed_identity_files() {
        // `ensure_layout` is the dir-skeleton hook; it must not write
        // identity content. (The on-demand seeding now lives in
        // `load_identity_files`; the contract guarded here is just
        // that `ensure_layout` itself is purely about directories.)
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();

        let mgr = WorkspaceManager::new(dir.clone());
        mgr.ensure_layout().await.expect("layout");

        let paths = WorkspacePaths::new(dir.clone());
        for kind in IdentityKind::all() {
            assert!(
                !paths
                    .persona_identity_file(crate::paths::BUILTIN_PERSONA_DIR, kind)
                    .exists(),
                "ensure_layout must not create {}",
                kind.file_name()
            );
        }
    }

    #[tokio::test]
    async fn seed_default_identity_files_writes_each_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();

        let mgr = WorkspaceManager::new(dir.clone());
        mgr.ensure_layout().await.expect("layout");
        mgr.seed_default_identity_files().await.expect("seed");

        let loaded = mgr.load_identity_files().await.expect("load");
        assert_eq!(loaded.soul, IdentityKind::Soul.default_content());
        assert_eq!(loaded.user, IdentityKind::User.default_content());
        assert_eq!(loaded.identity, IdentityKind::Identity.default_content());
    }

    #[tokio::test]
    async fn seed_default_identity_files_preserves_user_edits() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();

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
        assert_eq!(loaded.soul, CUSTOM);
        assert_eq!(loaded.identity, IdentityKind::Identity.default_content());
    }
}
