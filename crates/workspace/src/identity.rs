use std::path::{Path, PathBuf};

/// Author on the commits Baybo makes into `personas/`. Matches the `Edit`
/// tool's audit author, so `git log` reads as one voice.
const GIT_AUTHOR_NAME: &str = "Baybo";
const GIT_AUTHOR_EMAIL: &str = "baybo@local";

pub use crate::paths::IdentityKind;
use crate::paths::WorkspacePaths;

/// Contents of the workspace identity files. Every field is always
/// populated: [`load_identity_files`] seeds any missing file from its
/// [`IdentityKind::default_content`] before returning, so callers
/// never have to branch on a `None`.
#[derive(Debug, Clone)]
pub struct IdentityFiles {
    /// SOUL.md - personality, tone, and preferences.
    pub soul: String,
    /// IDENTITY.md - the agent's self-image: name, creature, vibe, emoji,
    /// avatar.
    pub identity: String,
    /// This agent's own USER.md - what it has worked out about the human.
    pub user: String,
    /// The shared `personas/USER.md` every agent reads: the stable facts the
    /// operator curates. Owned by no agent, so it is always a section of its
    /// own.
    pub shared_user: String,
}

/// Read `path`; if it is missing, seed it with `default` and return
/// the same default. The write is staged via a sibling `.tmp` file and
/// renamed so a concurrent reader observes either the previous state
/// or the full default — never a partial.
async fn read_or_seed(path: &Path, default: &str) -> anyhow::Result<String> {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => Ok(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let tmp = path.with_extension("md.tmp");
            tokio::fs::write(&tmp, default).await?;
            tokio::fs::rename(&tmp, path).await?;
            Ok(default.to_string())
        }
        Err(e) => Err(e.into()),
    }
}

/// Where one identity section is read from, and what to create it with if
/// it is absent.
///
/// A parameter rather than a fixed path because `SOUL.md` and `IDENTITY.md`
/// belong to the *agent*: a session bound to a custom agent reads them from
/// `personas/<id>/`, and the built-in from `personas/baybo/`.
#[derive(Clone, Copy)]
pub struct IdentitySource<'a> {
    pub path: &'a Path,
    pub seed: &'a str,
}

impl<'a> IdentitySource<'a> {
    pub fn new(path: &'a Path, seed: &'a str) -> Self {
        Self { path, seed }
    }
}

/// The built-in agent's own three files, under `personas/baybo/`. Returned
/// as paths because [`IdentitySource`] borrows them.
pub fn builtin_identity_paths(paths: &WorkspacePaths) -> (PathBuf, PathBuf, PathBuf) {
    let file = |kind| paths.persona_identity_file(crate::paths::BUILTIN_PERSONA_DIR, kind);
    (
        file(IdentityKind::Soul),
        file(IdentityKind::Identity),
        file(IdentityKind::User),
    )
}

/// Read one identity file, seeding it with `source.seed` if it does not
/// exist yet.
pub async fn load_identity(source: IdentitySource<'_>) -> anyhow::Result<String> {
    read_or_seed(source.path, source.seed).await
}

/// Load the identity sections. All three per-agent files come from the
/// caller-supplied sources; the shared `personas/USER.md` is read here and
/// always returned, because it belongs to no agent — every one of them reads
/// it alongside its own notes.
///
/// Auto-seeding here means a deleted identity file is recreated on the
/// next session boot. That matches what we want for runtime correctness
/// (the Soul prompt is never half-formed), but it does mean
/// "delete to disable" is not a way to opt out of identity injection —
/// edit the file to be empty instead.
pub async fn load_identity_files(
    root: &Path,
    soul: IdentitySource<'_>,
    identity: IdentitySource<'_>,
    user: IdentitySource<'_>,
) -> anyhow::Result<IdentityFiles> {
    let paths = WorkspacePaths::new(root.to_path_buf());
    let shared_user_path = paths.shared_user_file();
    let (soul, identity, own_user) = tokio::try_join!(
        read_or_seed(soul.path, soul.seed),
        read_or_seed(identity.path, identity.seed),
        read_or_seed(user.path, user.seed),
    )?;
    let shared_user = read_or_seed(&shared_user_path, IdentityKind::User.default_content()).await?;

    Ok(IdentityFiles {
        soul,
        identity,
        user: own_user,
        shared_user,
    })
}

/// Materialize one agent's persona directory: create
/// `personas/<agent_id>/skills/`, then write `SOUL.md`, `IDENTITY.md` and
/// `USER.md` **only if absent**, each staged through a sibling `.tmp` file
/// and renamed.
///
/// Idempotent and never destructive — files the agent has since rewritten
/// survive every later call. Run at profile creation and again defensively
/// when an actor for a bound session is built, which is what covers rows
/// created before this existed and files an operator deleted.
///
/// Finishes by committing the directory into the `personas/` repo, so the
/// agent's own later edits show up as diffs against a real baseline rather
/// than as file additions.
pub async fn ensure_persona_layout(
    paths: &WorkspacePaths,
    agent_id: &str,
    seed_soul: &str,
) -> anyhow::Result<()> {
    let skills = paths.persona_skills_dir(agent_id);
    tokio::fs::create_dir_all(&skills)
        .await
        .map_err(|e| anyhow::anyhow!("create persona skills dir {}: {e}", skills.display()))?;
    for (kind, seed) in [
        (IdentityKind::Soul, seed_soul),
        // The self-image template is seeded verbatim: it invites the agent to
        // pick its own name and emoji, and pre-filling it from the profile row
        // would only mint a copy that goes stale on the next rename.
        (
            IdentityKind::Identity,
            IdentityKind::Identity.default_content(),
        ),
    ] {
        let path = paths.persona_identity_file(agent_id, kind);
        read_or_seed(&path, seed)
            .await
            .map_err(|e| anyhow::anyhow!("seed persona {}: {e}", path.display()))?;
    }
    commit_personas(paths, agent_id, &format!("personas: baseline {agent_id}")).await;
    Ok(())
}

/// Commit whatever is untracked or modified under `personas/<pathspec>`, so
/// those files have a baseline in git before an agent starts rewriting them.
///
/// Without this a persona's files stay untracked until the agent's first
/// `Edit`, and that commit then reads as an *addition* — so the one thing the
/// audit trail exists for, seeing what the agent changed, is unavailable for
/// exactly the first change it makes. Idempotent: with nothing staged it
/// commits nothing.
///
/// Best-effort. A workspace without `git`, or a `personas/` that is not a
/// repo, must not stop an agent from being created — the files are already on
/// disk and correct; only their history is missing.
pub(crate) async fn commit_personas(paths: &WorkspacePaths, pathspec: &str, subject: &str) {
    let repo = paths.personas_dir();
    if !repo.join(".git").exists() {
        return;
    }
    let git = |args: Vec<String>| {
        let repo = repo.clone();
        async move {
            tokio::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .await
                .ok()
                .filter(|out| out.status.success())
        }
    };
    if git(vec!["add".into(), "--".into(), pathspec.to_owned()])
        .await
        .is_none()
    {
        return;
    }
    // An empty commit would be noise on every materialisation — and
    // materialisation runs defensively on every bound spawn.
    if git(vec!["diff".into(), "--cached".into(), "--quiet".into()])
        .await
        .is_some()
    {
        return;
    }
    let _ = git(vec![
        "-c".into(),
        format!("user.name={GIT_AUTHOR_NAME}"),
        "-c".into(),
        format!("user.email={GIT_AUTHOR_EMAIL}"),
        "commit".into(),
        "--no-verify".into(),
        "--quiet".into(),
        "-m".into(),
        subject.to_owned(),
        "--".into(),
        pathspec.to_owned(),
    ])
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A persona must have a baseline in git the moment it exists —
    /// otherwise the agent's first rewrite of its own soul lands as a file
    /// addition, and the audit trail cannot show what changed.
    #[tokio::test]
    async fn materialising_a_persona_commits_a_baseline() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());
        let personas = paths.personas_dir();
        tokio::fs::create_dir_all(&personas).await.unwrap();
        let git = async |args: Vec<&str>| {
            tokio::process::Command::new("git")
                .arg("-C")
                .arg(&personas)
                .args(args)
                .output()
                .await
                .unwrap()
        };
        git(vec!["init", "--quiet", "-b", "main"]).await;

        ensure_persona_layout(&paths, "01JAGENT", "SEED")
            .await
            .unwrap();

        let status = git(vec!["status", "--porcelain"]).await;
        assert!(
            String::from_utf8_lossy(&status.stdout).trim().is_empty(),
            "nothing may be left untracked: {}",
            String::from_utf8_lossy(&status.stdout),
        );
        let log = git(vec!["log", "--format=%an <%ae>%n%s", "--name-only"]).await;
        let log = String::from_utf8_lossy(&log.stdout);
        assert!(log.contains("Baybo <baybo@local>"), "{log}");
        assert!(log.contains("personas: baseline 01JAGENT"), "{log}");
        assert!(log.contains("01JAGENT/SOUL.md"), "{log}");

        // Idempotent: a second materialisation has nothing to say.
        ensure_persona_layout(&paths, "01JAGENT", "SEED")
            .await
            .unwrap();
        let count = git(vec!["rev-list", "--count", "HEAD"]).await;
        assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "1");
    }

    /// Load the workspace's own three sections — i.e. what the built-in
    /// agent reads, all three coming from `personas/baybo/`.
    async fn load_workspace(dir: &Path) -> IdentityFiles {
        let paths = WorkspacePaths::new(dir.to_path_buf());
        let (soul, identity, user) = builtin_identity_paths(&paths);
        load_identity_files(
            dir,
            IdentitySource::new(&soul, IdentityKind::Soul.default_content()),
            IdentitySource::new(&identity, IdentityKind::Identity.default_content()),
            IdentitySource::new(&user, IdentityKind::User.default_content()),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn load_seeds_missing_files_with_defaults() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test_tmp")
            .join("workspace_identity_test");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let profile = dir.join("personas").join(crate::paths::BUILTIN_PERSONA_DIR);
        tokio::fs::create_dir_all(&profile).await.unwrap();
        // Only SOUL.md is hand-written; USER.md and IDENTITY.md are absent.
        tokio::fs::write(profile.join("SOUL.md"), "You are helpful.")
            .await
            .unwrap();

        let files = load_workspace(&dir).await;
        assert_eq!(files.soul, "You are helpful.");
        assert_eq!(files.user, IdentityKind::User.default_content());
        assert_eq!(files.identity, IdentityKind::Identity.default_content());

        // After load, the missing files must exist on disk so subsequent
        // direct readers (e.g. the Edit tool) see the same content.
        assert_eq!(
            tokio::fs::read_to_string(profile.join("USER.md"))
                .await
                .unwrap(),
            IdentityKind::User.default_content(),
        );
        assert_eq!(
            tokio::fs::read_to_string(profile.join("IDENTITY.md"))
                .await
                .unwrap(),
            IdentityKind::Identity.default_content(),
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn load_on_fresh_workspace_seeds_all_three() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test_tmp")
            .join("identity_fresh_seed_test");
        let _ = tokio::fs::remove_dir_all(&dir).await;

        // No persona dir at all yet — load must create it and seed
        // every file.
        let files = load_workspace(&dir).await;
        assert_eq!(files.soul, IdentityKind::Soul.default_content());
        assert_eq!(files.user, IdentityKind::User.default_content());
        assert_eq!(files.identity, IdentityKind::Identity.default_content());

        let profile = dir.join("personas").join(crate::paths::BUILTIN_PERSONA_DIR);
        for kind in IdentityKind::all() {
            assert!(profile.join(kind.file_name()).exists());
        }

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
