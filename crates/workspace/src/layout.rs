//! Materialising the workspace skeleton: the two boot-time writes that turn a
//! bare root into a usable workspace. Free functions over [`WorkspacePaths`]
//! for the same reason `identity` and `singleton` are — the address type is
//! already the thing every caller holds, and wrapping it in a handle only
//! meant re-deriving the paths on the other side.

use std::path::Path;

use crate::identity;
use crate::paths::{IdentityKind, WorkspacePaths};

/// Materialise the workspace skeleton: create `config/`,
/// `skills/`, `agents/`, `personas/`, `.key/`, `state/`, `work/`,
/// `work/tmp/`, `logs/`, and initialise a standalone git repo inside
/// each of the declarative dirs if it isn't one already. Idempotent —
/// safe to call on every boot.
pub async fn ensure_layout(paths: &WorkspacePaths) -> anyhow::Result<()> {
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
/// Assumes `personas/` already exists (i.e. `ensure_layout` has run).
pub async fn seed_default_identity_files(paths: &WorkspacePaths) -> anyhow::Result<()> {
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
    let mut seeded_any = false;
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
        tokio::fs::write(&target, default)
            .await
            .map_err(|e| anyhow::anyhow!("seed default identity file {}: {e}", target.display()))?;
        seeded_any = true;
    }
    if seeded_any {
        // Same reason the per-agent materialisation commits: a file that
        // enters git only when the agent first rewrites it makes that
        // first rewrite unreadable.
        identity::commit_personas(paths, ".", "personas: seed defaults").await;
    }
    Ok(())
}

/// Initialise a standalone git repository inside `dir` if one isn't
/// already there. Idempotent — a no-op when `<dir>/.git` exists.
///
/// These repos exist only for optional version history of the agent's
/// skills/personas/agents. The `bench-bash` build skips them (see the no-op
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

    async fn read_builtin(root: &Path, kind: IdentityKind) -> String {
        let path = WorkspacePaths::new(root.to_path_buf())
            .persona_identity_file(crate::paths::BUILTIN_PERSONA_DIR, kind);
        tokio::fs::read_to_string(&path)
            .await
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    #[tokio::test]
    async fn ensure_layout_creates_dirs_and_local_git_repos() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();
        let paths = WorkspacePaths::new(dir.clone());
        ensure_layout(&paths).await.expect("layout");

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
        // Each of config/, personas/, skills/, and agents/ is its own git repo.
        assert!(paths.config_dir().join(".git").is_dir());
        assert!(paths.skills_dir().join(".git").is_dir());
        assert!(paths.agents_dir().join(".git").is_dir());
        assert!(paths.personas_dir().join(".git").is_dir());
        // .key/ is NOT a git repo — encryption key must never be tracked.
        assert!(!paths.key_dir().join(".git").exists());

        // Idempotent: a re-apply must not re-init or fail.
        ensure_layout(&paths).await.expect("layout reapply");
        assert!(paths.config_dir().join(".git").is_dir());
        assert!(paths.skills_dir().join(".git").is_dir());
    }

    #[tokio::test]
    async fn ensure_layout_does_not_seed_identity_files() {
        // `ensure_layout` is the dir-skeleton hook; it must not write
        // identity content — seeding is `seed_default_identity_files`, and
        // a read that finds nothing seeds on demand.
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());
        ensure_layout(&paths).await.expect("layout");
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

        let paths = WorkspacePaths::new(dir.clone());
        ensure_layout(&paths).await.expect("layout");
        seed_default_identity_files(&paths).await.expect("seed");

        for kind in IdentityKind::all() {
            assert_eq!(read_builtin(&dir, kind).await, kind.default_content());
        }
        assert_eq!(
            tokio::fs::read_to_string(paths.shared_user_file())
                .await
                .expect("shared USER.md"),
            IdentityKind::User.default_content(),
        );
    }

    #[tokio::test]
    async fn seed_default_identity_files_preserves_user_edits() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();

        let paths = WorkspacePaths::new(dir.clone());
        ensure_layout(&paths).await.expect("first layout");
        seed_default_identity_files(&paths)
            .await
            .expect("first seed");

        const CUSTOM: &str = "# Soul\n\nHand-edited by the operator.\n";
        let soul =
            paths.persona_identity_file(crate::paths::BUILTIN_PERSONA_DIR, IdentityKind::Soul);
        tokio::fs::write(&soul, CUSTOM)
            .await
            .expect("operator edit");

        // A second seed must keep the operator edit intact and leave
        // the rest of the defaults alone.
        seed_default_identity_files(&paths).await.expect("re-seed");
        assert_eq!(read_builtin(&dir, IdentityKind::Soul).await, CUSTOM);
        assert_eq!(
            read_builtin(&dir, IdentityKind::Identity).await,
            IdentityKind::Identity.default_content(),
        );
    }
}
