//! Materialising the workspace skeleton: the two boot-time writes that turn a
//! bare root into a usable workspace. Free functions over [`WorkspacePaths`]
//! for the same reason `identity` and `singleton` are — the address type is
//! already the thing every caller holds, and wrapping it in a handle only
//! meant re-deriving the paths on the other side.

use std::path::Path;

use crate::identity;
use crate::paths::{IdentityKind, WorkspacePaths};

/// Materialise the workspace skeleton: create `config/`, `agents/`,
/// `personas/`, `personas/baybo/skills/`, `.key/`, `state/`, `work/`,
/// `work/tmp/`, `logs/`, and initialise a standalone git repo inside
/// each of the declarative dirs if it isn't one already. Idempotent —
/// safe to call on every boot.
///
/// `personas/baybo/skills/` is the one persona-internal path created here,
/// and it is created **empty on purpose**. `SkillRegistry` only records a
/// directory that exists, and `reload()` replays exactly the recorded list —
/// while the operator dashboard's refresh calls `reload()` and nothing else.
/// So a default scope whose directory did not exist yet would silently ignore
/// a hand-placed skill until the next restart. Every other agent gets the
/// same guarantee from `ensure_persona_layout` at profile creation; the
/// built-in has no such hook, because its id is a constant rather than DB
/// state, so its directory is layout's job.
///
/// Also writes `personas/.gitignore` — see [`PERSONAS_GITIGNORE_BODY`].
pub async fn ensure_layout(paths: &WorkspacePaths) -> anyhow::Result<()> {
    for dir in [
        paths.config_dir(),
        paths.persona_skills_dir(crate::paths::BUILTIN_PERSONA_DIR),
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

    for dir in [paths.config_dir(), paths.agents_dir(), paths.personas_dir()] {
        ensure_git_repo(&dir).await?;
    }
    ensure_personas_gitignore(paths).await?;
    Ok(())
}

/// Everything under `personas/` that is transient rather than declarative.
///
/// `SkillInstall` stages a copy at `<agent>/skills/.staging/<uuid>/` before
/// the atomic rename, and a crash between the two leaves it there. That used
/// to land in a skills-only repo nobody hand-curated; it now lands in the one
/// repo an operator is told to commit their personas from, where a plain
/// `git add -A` would sweep it in. Same reason `deck` ignores its own
/// `.staging/`.
const PERSONAS_GITIGNORE_BODY: &str = "*/skills/.staging/
";

/// Write `personas/.gitignore` if it is missing. Never overwritten — an
/// operator who edited it keeps their version, exactly like the identity
/// files.
async fn ensure_personas_gitignore(paths: &WorkspacePaths) -> anyhow::Result<()> {
    let path = paths.personas_dir().join(".gitignore");
    if tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Ok(());
    }
    tokio::fs::write(&path, PERSONAS_GITIGNORE_BODY)
        .await
        .map_err(|e| anyhow::anyhow!("write {}: {e}", path.display()))
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
                identity::persona_seed(crate::paths::BUILTIN_PERSONA_DIR, kind),
            )
        })
        .into_iter()
        // The shared profile is nobody's persona file, so it is the one target
        // here that does not come from the per-agent seed table.
        .chain([(
            paths.shared_user_file(),
            IdentityKind::User.default_content(),
        )])
        // The built-in's memory index. `ensure_persona_layout` seeds this for
        // every *custom* agent and skips the built-in, so without it here the
        // built-in's index appears lazily at first assembly and never enters
        // the baseline — making its first real change read as the file being
        // created, which is the one thing the baseline exists to prevent.
        .chain([(
            paths.persona_memory_index_file(crate::paths::BUILTIN_PERSONA_DIR),
            crate::prompt::MEMORY_INDEX_TEMPLATE,
        )]);
    let mut seeded: Vec<String> = Vec::new();
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
        if let Ok(rel) = target.strip_prefix(paths.personas_dir()) {
            seeded.push(rel.to_string_lossy().into_owned());
        }
    }
    if !seeded.is_empty() {
        // Same reason the per-agent materialisation commits: a file that
        // enters git only when the agent first rewrites it makes that first
        // rewrite unreadable. Only what this call wrote — an operator's
        // uncommitted edit to a neighbouring file is not part of "seed
        // defaults".
        let specs: Vec<&str> = seeded.iter().map(String::as_str).collect();
        identity::commit_personas(paths, &specs, "personas: seed defaults").await;
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
            paths.persona_skills_dir(crate::paths::BUILTIN_PERSONA_DIR),
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
        // Each of config/, personas/, and agents/ is its own git repo.
        assert!(paths.config_dir().join(".git").is_dir());
        assert!(paths.agents_dir().join(".git").is_dir());
        assert!(paths.personas_dir().join(".git").is_dir());
        // Skills are versioned by the personas/ repo they sit in; a nested
        // .git there would give git two owners for one file.
        assert!(
            !paths
                .persona_skills_dir(crate::paths::BUILTIN_PERSONA_DIR)
                .join(".git")
                .exists()
        );
        // .key/ is NOT a git repo — encryption key must never be tracked.
        assert!(!paths.key_dir().join(".git").exists());

        // A leaked `SkillInstall` staging tree must not be sweepable into the
        // personas repo by a plain `git add -A`.
        let ignore = paths.personas_dir().join(".gitignore");
        assert!(ignore.is_file(), "missing {}", ignore.display());
        let staging = paths
            .persona_skills_dir(crate::paths::BUILTIN_PERSONA_DIR)
            .join(".staging/abc-123");
        tokio::fs::create_dir_all(&staging).await.expect("mkdir");
        tokio::fs::write(staging.join("SKILL.md"), "x")
            .await
            .expect("write");
        let out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(paths.personas_dir())
            .args(["status", "--porcelain", "--untracked-files=all"])
            .output()
            .await
            .expect("git status");
        let status = String::from_utf8_lossy(&out.stdout);
        assert!(
            !status.contains(".staging"),
            "staging tree is visible to `git add -A`: {status}"
        );

        // An operator's own edit to it survives a re-apply.
        tokio::fs::write(&ignore, "# mine\n").await.expect("edit");

        // Idempotent: a re-apply must not re-init or fail.
        ensure_layout(&paths).await.expect("layout reapply");
        assert_eq!(
            tokio::fs::read_to_string(&ignore).await.expect("read"),
            "# mine\n",
            "operator edit clobbered"
        );
        assert!(paths.config_dir().join(".git").is_dir());
        assert!(paths.personas_dir().join(".git").is_dir());
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
    async fn seed_default_identity_files_seeds_the_builtins_memory_index() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());
        ensure_layout(&paths).await.expect("layout");
        seed_default_identity_files(&paths).await.expect("seed");

        // The built-in is skipped by `ensure_persona_layout`, so this is its
        // only chance to enter the baseline alongside its identity files.
        let index = paths.persona_memory_index_file(crate::paths::BUILTIN_PERSONA_DIR);
        assert!(index.is_file(), "missing {}", index.display());
        let tracked = tokio::process::Command::new("git")
            .arg("-C")
            .arg(paths.personas_dir())
            .args(["ls-files", "--error-unmatch", "baybo/memory/MEMORY.md"])
            .status()
            .await
            .expect("git ls-files");
        assert!(
            tracked.success(),
            "the index must be in the baseline commit"
        );
    }

    #[tokio::test]
    async fn seed_default_identity_files_writes_each_default() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().to_path_buf();

        let paths = WorkspacePaths::new(dir.clone());
        ensure_layout(&paths).await.expect("layout");
        seed_default_identity_files(&paths).await.expect("seed");

        assert_eq!(
            read_builtin(&dir, IdentityKind::Soul).await,
            IdentityKind::Soul.default_content()
        );
        assert_eq!(
            read_builtin(&dir, IdentityKind::Identity).await,
            IdentityKind::Identity.default_content()
        );
        let shared = tokio::fs::read_to_string(paths.shared_user_file())
            .await
            .expect("shared USER.md");
        assert_eq!(shared, IdentityKind::User.default_content());
        // The built-in's own notes are a different document from the shared
        // profile. Seeding both from the same default made every prompt carry
        // two byte-identical user sections.
        let own = read_builtin(&dir, IdentityKind::User).await;
        assert_eq!(own, crate::prompt::PERSONA_USER_TEMPLATE);
        assert_ne!(own, shared);
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
