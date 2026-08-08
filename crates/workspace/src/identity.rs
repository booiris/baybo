use std::path::Path;

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
pub(crate) async fn read_or_seed(path: &Path, default: &str) -> anyhow::Result<String> {
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

/// The seed for one agent's identity file.
///
/// One table, consulted by every writer — setup, profile creation, and the
/// on-demand seed a prompt assembly does when a file is missing. They used to
/// each carry their own answer, which meant deleting the built-in's `SOUL.md`
/// silently replaced its persona with the custom-agent skeleton on the next
/// turn, and deleting its `USER.md` reintroduced a duplicate of the shared
/// profile.
///
/// The built-in ships a real persona; a custom agent starts from a skeleton
/// that invites it to write its own. `USER.md` is per-agent for both — the
/// operator-curated defaults belong to the shared `personas/USER.md`, which is
/// not an agent's file and is not seeded here.
pub fn persona_seed(agent_id: &str, kind: IdentityKind) -> &'static str {
    let builtin = agent_id == crate::paths::BUILTIN_PERSONA_DIR;
    match (kind, builtin) {
        (IdentityKind::Soul, true) => IdentityKind::Soul.default_content(),
        (IdentityKind::Soul, false) => crate::prompt::PERSONA_SOUL_TEMPLATE,
        (IdentityKind::User, _) => crate::prompt::PERSONA_USER_TEMPLATE,
        // The self-image template is seeded verbatim for everyone: it invites
        // the agent to pick its own name and emoji, and pre-filling it from a
        // profile row would only mint a copy that goes stale on the next
        // rename.
        (IdentityKind::Identity, _) => IdentityKind::Identity.default_content(),
    }
}

/// Materialize one agent's resolved persona directory: create its `skills/`
/// and `memory/`, then write `SOUL.md`, `IDENTITY.md`, `USER.md` and an empty
/// `MEMORY.md` index **only if absent**, each staged through a sibling `.tmp`
/// file and renamed.
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
    let persona = paths.persona_dir(agent_id);
    ensure_persona_layout_at(paths, agent_id, seed_soul, &persona).await
}

async fn ensure_persona_layout_at(
    paths: &WorkspacePaths,
    agent_id: &str,
    seed_soul: &str,
    persona: &Path,
) -> anyhow::Result<()> {
    let relative = persona
        .strip_prefix(paths.personas_dir())
        .map_err(|e| anyhow::anyhow!("resolve persona path {}: {e}", persona.display()))?
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("persona path is not valid UTF-8: {}", persona.display()))?;
    let skills = persona.join(crate::paths::SKILLS_DIR);
    tokio::fs::create_dir_all(&skills)
        .await
        .map_err(|e| anyhow::anyhow!("create persona skills dir {}: {e}", skills.display()))?;
    let mut seeded: Vec<String> = Vec::new();
    // The memory index joins the baseline for the same reason the identity
    // files do: its first real change should read as a change, not as the
    // file appearing out of nowhere.
    let memory_index = persona
        .join(crate::paths::PERSONA_MEMORY_DIR)
        .join(crate::paths::MEMORY_INDEX_FILE);
    let memory_existed = tokio::fs::try_exists(&memory_index).await.unwrap_or(false);
    crate::memory::load_memory_index(&memory_index).await?;
    if !memory_existed {
        seeded.push(format!(
            "{relative}/{}/{}",
            crate::paths::PERSONA_MEMORY_DIR,
            crate::paths::MEMORY_INDEX_FILE
        ));
    }
    for kind in IdentityKind::all() {
        let seed = match kind {
            // The caller's soul only applies to the file the caller is
            // creating; every other kind takes the shared seed table.
            IdentityKind::Soul => seed_soul,
            other => persona_seed(agent_id, other),
        };
        let path = persona.join(kind.file_name());
        let existed = tokio::fs::try_exists(&path).await.unwrap_or(false);
        read_or_seed(&path, seed)
            .await
            .map_err(|e| anyhow::anyhow!("seed persona {}: {e}", path.display()))?;
        if !existed {
            seeded.push(format!("{relative}/{}", kind.file_name()));
        }
    }
    // Only the files this call created. The pathspec is what git stages, so
    // passing the whole directory would sweep an agent's in-progress edit — or
    // a hand-authored skill — into a commit labelled as a baseline. This runs
    // defensively on every bound-session spawn, so that would be routine.
    if !seeded.is_empty() {
        let specs: Vec<&str> = seeded.iter().map(String::as_str).collect();
        commit_personas(paths, &specs, &format!("personas: baseline {agent_id}")).await;
    }
    Ok(())
}

/// Materialise a project-owned persona under `personas/project/` and give it
/// a name in one step, for an agent the system is creating on the operator's
/// behalf rather than at their keyboard — a project's lead or teammate.
///
/// Two writes, in the order that matters: [`ensure_persona_layout`] seeds
/// the files and commits the baseline, then the name is spliced into the
/// `IDENTITY.md` that call just wrote and committed on top. Doing it the
/// other way round would show the name as part of the baseline, so a later
/// rename would read as the *first* time the agent was ever named.
///
/// Unlocked on purpose: `agent_id` was minted a moment ago and nothing else
/// in the process can be holding this file yet. Every *edit* path — the
/// gateway's rename, the agent's own rewrite — still goes through
/// [`personas_git_lock`] and the identity write lock.
pub async fn ensure_named_persona_layout(
    paths: &WorkspacePaths,
    agent_id: &str,
    seed_soul: &str,
    display_name: &str,
) -> anyhow::Result<()> {
    if !crate::paths::is_project_persona_id(agent_id) {
        anyhow::bail!(
            "project persona id {agent_id:?} must start with {:?}",
            crate::paths::PROJECT_PERSONA_ID_PREFIX
        );
    }
    let persona = paths.project_persona_dir(agent_id);
    ensure_persona_layout_at(paths, agent_id, seed_soul, &persona).await?;
    let path = persona.join(IdentityKind::Identity.file_name());
    let current = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
    let named = crate::name::with_display_name(&current, display_name);
    if named == current {
        return Ok(());
    }
    let tmp = path.with_extension("md.tmp");
    tokio::fs::write(&tmp, &named)
        .await
        .map_err(|e| anyhow::anyhow!("write {}: {e}", tmp.display()))?;
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(|e| anyhow::anyhow!("rename into {}: {e}", path.display()))?;
    commit_personas(
        paths,
        &[&format!(
            "{}/{agent_id}/{}",
            crate::paths::PROJECT_PERSONAS_DIR,
            IdentityKind::Identity.file_name()
        )],
        &format!("personas: name {agent_id}"),
    )
    .await;
    Ok(())
}

/// Serialises everything that runs `git` against `personas/`.
///
/// `add` + `commit` hold `.git/index.lock` for their whole run, and a `git`
/// that loses the race exits rather than waiting. Two writers reach this
/// repo — the baseline commits here and the per-write audit commits in
/// `baybo-tools` — and the dream pass makes them concurrent by opening one
/// fire per agent. The lock lives in this crate because it is the lower of
/// the two, so both can share one.
///
/// Process-global because the repo is: one workspace, one `personas/`.
///
/// `tokio::sync::Mutex` rather than the `parking_lot` the style guide
/// mandates everywhere else, and deliberately: this guard is **held across
/// `.await`** — the whole point is to span a `git` subprocess — so a
/// blocking mutex here would park a runtime worker for the length of a
/// process spawn, and a second waiter on the same thread would deadlock.
/// This is the exception the rule leaves room for, not an oversight to fix.
pub fn personas_git_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
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
pub async fn commit_personas(paths: &WorkspacePaths, pathspecs: &[&str], subject: &str) {
    let repo = paths.personas_dir();
    if !repo.join(".git").exists() {
        return;
    }
    let _guard = personas_git_lock().lock().await;
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
    let specs: Vec<String> = pathspecs.iter().map(|s| (*s).to_owned()).collect();
    let mut add = vec!["add".to_owned(), "--".to_owned()];
    add.extend(specs.iter().cloned());
    if git(add).await.is_none() {
        return;
    }
    // Scope the emptiness check to the same pathspecs as the commit: with a
    // narrow pathspec an unrelated dirty file would otherwise make this look
    // like there is something to commit, and the commit would then be empty.
    let mut staged = vec![
        "diff".to_owned(),
        "--cached".to_owned(),
        "--quiet".to_owned(),
        "--".to_owned(),
    ];
    staged.extend(specs.iter().cloned());
    if git(staged).await.is_some() {
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
    ]
    .into_iter()
    .chain(specs)
    .collect())
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A persona must have a baseline in git the moment it exists —
    /// otherwise the agent's first rewrite of its own soul lands as a file
    /// addition, and the audit trail cannot show what changed.
    /// The persona is three files, not two. `USER.md` left to the first
    /// prompt assembly would miss the baseline commit below it, which is the
    /// one thing that makes the agent's first rewrite of its own notes
    /// readable as a diff.
    #[tokio::test]
    async fn materialising_seeds_all_three_identity_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());
        ensure_persona_layout(&paths, "01JTHREE", "soul body")
            .await
            .expect("materialise");
        for kind in IdentityKind::all() {
            let path = paths.persona_identity_file("01JTHREE", kind);
            assert!(path.exists(), "missing {}", path.display());
        }
        assert!(paths.persona_skills_dir("01JTHREE").is_dir());
    }

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

    #[tokio::test]
    async fn materialising_a_named_project_persona_groups_it_under_project() {
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

        let agent_id = "project-01JLEAD";
        ensure_named_persona_layout(&paths, agent_id, "LEAD SOUL", "Lead")
            .await
            .unwrap();

        let project = paths.project_persona_dir(agent_id);
        assert!(project.join(IdentityKind::Soul.file_name()).exists());
        assert_eq!(paths.persona_dir(agent_id), project);
        assert!(!paths.personas_dir().join(agent_id).exists());
        let identity = tokio::fs::read_to_string(
            paths.persona_identity_file(agent_id, IdentityKind::Identity),
        )
        .await
        .unwrap();
        assert_eq!(
            crate::name::display_name(&identity).as_deref(),
            Some("Lead")
        );

        let status = git(vec!["status", "--porcelain"]).await;
        assert!(
            String::from_utf8_lossy(&status.stdout).trim().is_empty(),
            "nothing may be left untracked: {}",
            String::from_utf8_lossy(&status.stdout),
        );
        let subjects = git(vec!["log", "--format=%s"]).await;
        let subjects = String::from_utf8_lossy(&subjects.stdout);
        let subjects: Vec<&str> = subjects.lines().collect();
        assert_eq!(
            subjects,
            [
                "personas: name project-01JLEAD",
                "personas: baseline project-01JLEAD"
            ]
        );
        let log = git(vec!["log", "--format=%s", "--name-only"]).await;
        let log = String::from_utf8_lossy(&log.stdout);
        assert!(log.contains("project/project-01JLEAD/SOUL.md"), "{log}");
        assert!(log.contains("project/project-01JLEAD/IDENTITY.md"), "{log}");
    }

    /// Load the workspace's own three sections — i.e. what the built-in
    /// agent reads, all three coming from `personas/baybo/`.
    async fn load_workspace(dir: &Path) -> IdentityFiles {
        let paths = WorkspacePaths::new(dir.to_path_buf());
        let file = |kind| paths.persona_identity_file(crate::paths::BUILTIN_PERSONA_DIR, kind);
        let (soul, identity, user) = (
            file(IdentityKind::Soul),
            file(IdentityKind::Identity),
            file(IdentityKind::User),
        );
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
