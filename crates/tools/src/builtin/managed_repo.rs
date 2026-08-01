//! The parts of `personas/` an agent may write without an approval prompt,
//! and the git plumbing that makes that safe to allow.
//!
//! Two tiers, both inside the one `personas/` repo:
//!
//! - **Identity files** — `personas/<agent_id>/{SOUL,IDENTITY,USER}.md`. A
//!   closed allowlist: a persona directory is a declarative slot store.
//! - **Memory files** — anything under `personas/<agent_id>/memory/`. No
//!   filename allowlist: a memory tree is a freeform set of markdown files
//!   the agent names as it likes.
//!
//! Both share three guards: the approval bypass (the audit commit is the
//! accountability, not a per-write prompt), a size cap, and a commit into
//! `personas/` with a fixed `Baybo <baybo@local>` author so any write is
//! reviewable and revertible with plain `git`. `--no-verify` is intentional
//! — this is Baybo-managed audit history, not a hand-curated repo with
//! authored hooks. A commit failure never undoes the mutation; it surfaces
//! as a warning line in the tool output.
//!
//! And both are **owned**: an agent writes its own files only. The approval
//! declaration has no call context, so the bypass is decided on path *shape*
//! and ownership is enforced at execute time — a cross-agent write skips the
//! gate and is then refused outright, which writes nothing.

use std::path::{Path, PathBuf};

use baybo_model::AgentProfileId;
use baybo_workspace::paths::{
    PERSONA_MEMORY_DIR, PERSONAS_DIR, PersonaPath, SHARED_USER_FILE, classify_persona_path,
    escapes_upward, has_git_component,
};
use baybo_workspace::{WorkspacePaths, absolutise};
use tokio::process::Command;

use crate::ToolError;

type ToolResult<T> = crate::Result<T>;

const GIT_AUTHOR_NAME: &str = "Baybo";
const GIT_AUTHOR_EMAIL: &str = "baybo@local";

/// Size cap for a single audited file. Identity files are
/// system-prompt-bound and a memory file is one fact; either way a
/// multi-MiB blob is corruption or symlink shenanigans, and the cheap
/// check happens before anything is slurped into memory.
pub(crate) const MAX_MANAGED_FILE_BYTES: u64 = 1 << 20;

/// An audited write, resolved to the path to stage inside `personas/`.
pub(crate) struct ManagedTarget {
    /// Repo-relative, so `git add --` stages exactly this file:
    /// `<agent_id>/SOUL.md`, `<agent_id>/memory/fact.md`.
    pub(crate) rel_path: String,
}

/// What a commit records about a file.
#[derive(Clone, Copy)]
pub(crate) enum ChangeKind {
    Update,
    Remove,
}

impl ChangeKind {
    fn verb(self) -> &'static str {
        match self {
            Self::Update => "update",
            Self::Remove => "remove",
        }
    }
}

/// The absolutised workspace roots that get special write treatment.
/// Built once per tool, because `starts_with` against a relative root
/// silently never matches — the debug-build default workspace root is
/// `./.baybo`, while the paths the model passes come from the system
/// prompt and are absolute.
pub(crate) struct ManagedRoots {
    personas_dir: PathBuf,
    work_dir: PathBuf,
    skills_dir: PathBuf,
}

impl ManagedRoots {
    pub(crate) fn new(paths: &WorkspacePaths) -> Self {
        Self {
            personas_dir: absolutise(&paths.personas_dir()),
            work_dir: absolutise(&paths.work_dir()),
            skills_dir: absolutise(&paths.skills_dir()),
        }
    }

    /// The repo every audited write is committed into.
    pub(crate) fn personas_dir(&self) -> &Path {
        &self.personas_dir
    }

    /// Whether a write here skips the approval gate: an identity file, a
    /// memory file, or anything under `work/` / `skills/`.
    ///
    /// Shape only — *some* agent's file. Ownership is checked at execute
    /// time, because this has no call context to check it against.
    pub(crate) fn bypasses_approval(&self, file_path: &Path) -> bool {
        self.is_identity_shape(file_path)
            || self.is_memory_shape(file_path)
            || self.is_inside(&self.work_dir, file_path)
            || self.is_inside(&self.skills_dir, file_path)
    }

    /// What `file_path` is, or [`PersonaPath::Other`] when it is not a
    /// recognised file under `personas/` at all.
    fn shape<'a>(&self, file_path: &'a Path) -> PersonaPath<'a> {
        match self.locate(&self.personas_dir, file_path) {
            Some(rel) => classify_persona_path(rel),
            None => PersonaPath::Other,
        }
    }

    /// Whether `file_path` looks like an identity file: the shared
    /// `personas/USER.md`, or `personas/<any agent>/<IDENTITY>.md`.
    fn is_identity_shape(&self, file_path: &Path) -> bool {
        matches!(
            self.shape(file_path),
            PersonaPath::SharedUser | PersonaPath::Identity { .. }
        )
    }

    /// Whether `file_path` looks like `personas/<any agent>/memory/<file>`.
    pub(crate) fn is_memory_shape(&self, file_path: &Path) -> bool {
        matches!(self.shape(file_path), PersonaPath::Memory { .. })
    }

    /// `Write`'s narrower bypass: its own memory tree, or `work/`.
    ///
    /// Deliberately not [`Self::bypasses_approval`] — that one includes the
    /// identity shape, and a whole-file overwrite of a soul is the rare,
    /// deliberate act that should still meet the gate. It lives here rather
    /// than in the tool so the `..` / `.git` / symlink guards have one
    /// implementation instead of a third copy.
    pub(crate) fn write_bypasses_approval(&self, file_path: &Path) -> bool {
        self.is_memory_shape(file_path) || self.is_inside(&self.work_dir, file_path)
    }

    fn is_inside(&self, root: &Path, file_path: &Path) -> bool {
        self.locate(root, file_path).is_some()
    }

    /// The components of `file_path` below `root`, or `None` when it is not
    /// really in there.
    ///
    /// "Really" is the whole job. `Path::starts_with` is purely lexical, so
    /// membership is only as strong as what is checked around it:
    ///
    /// - `..` is refused outright — [`absolutise`] leaves it intact, so
    ///   `<personas>/../config/baybo.json` would otherwise satisfy the
    ///   prefix test.
    /// - a `.git` component is refused — `personas/` *is* a git repo, and
    ///   its own metadata must never be writable through the tier the repo
    ///   exists to audit. `core.fsmonitor` or a `filter.*.clean` command in
    ///   `.git/config` runs an arbitrary program on the very next `git add`,
    ///   which the audit commit itself performs; a deleted `.git/HEAD` turns
    ///   every later audit into a warning line.
    ///
    /// Symlinks are handled separately, by [`reject_symlinked_path`] at
    /// execute time, so the refusal can name the component that was a link.
    fn locate<'a>(&self, root: &Path, file_path: &'a Path) -> Option<&'a Path> {
        if !file_path.is_absolute() || escapes_upward(file_path) || has_git_component(file_path) {
            return None;
        }
        file_path.strip_prefix(root).ok()
    }

    /// The repo-relative pathspec `git add --` should stage, or `None` when
    /// the path is not one this repo can name.
    fn pathspec(&self, file_path: &Path) -> Option<String> {
        Some(
            self.locate(&self.personas_dir, file_path)?
                .to_str()?
                .to_string(),
        )
    }

    /// Resolve an audited write to the path that records it: memory file
    /// first, then identity file. `Ok(None)` when the path is in neither
    /// tier; `Err` when it is under `personas/` but is not something this
    /// agent may write.
    pub(crate) fn audit_target(
        &self,
        file_path: &Path,
        agent: &AgentProfileId,
    ) -> ToolResult<Option<ManagedTarget>> {
        if self.is_memory_shape(file_path) {
            return self.memory_target(file_path, agent);
        }
        self.identity_target(file_path, agent)
    }

    /// Resolve `personas/<agent_id>/{SOUL,IDENTITY,USER}.md` for the calling
    /// agent, rejecting a path under `personas/` that is not one of its own.
    /// `None` when the path is outside `personas/` altogether.
    pub(crate) fn identity_target(
        &self,
        file_path: &Path,
        agent: &AgentProfileId,
    ) -> ToolResult<Option<ManagedTarget>> {
        if !file_path.starts_with(&self.personas_dir) {
            return Ok(None);
        }
        // The shared human profile belongs to no agent, so every agent may
        // write it: what one of them learns about the person is worth the
        // others knowing. That does make it a channel between agents — the
        // one place the per-agent partition deliberately does not hold.
        let allowed = match self.shape(file_path) {
            PersonaPath::SharedUser => true,
            PersonaPath::Identity { agent_id, .. } => agent_id == agent.as_str(),
            PersonaPath::Memory { .. } | PersonaPath::Other => false,
        };
        if !allowed {
            return Err(ToolError::InvalidParams(format!(
                "writes under {PERSONAS_DIR}/ are restricted to the shared \
                 {SHARED_USER_FILE}, this agent's own {{SOUL,IDENTITY,USER}}.md, \
                 and its own {PERSONA_MEMORY_DIR}/; anything else there (a \
                 skills overlay, another agent's files) is not writable \
                 through a tool at all; got {}",
                file_path.display()
            )));
        }
        reject_symlinked_path(file_path, &self.personas_dir)?;
        Ok(Some(ManagedTarget {
            rel_path: self.pathspec(file_path).ok_or_else(unnameable)?,
        }))
    }

    /// Resolve a file under the calling agent's own memory tree,
    /// `personas/<agent_id>/memory/…`.
    ///
    /// Location-keyed with no filename allowlist — which is exactly why the
    /// guards in [`Self::locate`] and [`reject_symlinked_path`] carry the
    /// weight here that a filename allowlist carries for identity files.
    pub(crate) fn memory_target(
        &self,
        file_path: &Path,
        agent: &AgentProfileId,
    ) -> ToolResult<Option<ManagedTarget>> {
        let PersonaPath::Memory { agent_id } = self.shape(file_path) else {
            return Ok(None);
        };
        if agent_id != agent.as_str() {
            return Err(ToolError::InvalidParams(format!(
                "{} is another agent's memory; each agent keeps its own",
                file_path.display()
            )));
        }
        reject_symlinked_path(file_path, &self.personas_dir)?;
        Ok(Some(ManagedTarget {
            rel_path: self.pathspec(file_path).ok_or_else(unnameable)?,
        }))
    }
}

/// A path `classify_persona_path` recognised but `git` cannot be handed —
/// i.e. it is not valid UTF-8. Unreachable in practice (the classifier
/// refuses non-UTF-8 first), and a refusal rather than a lossy coercion.
fn unnameable() -> ToolError {
    ToolError::InvalidParams("path is not valid UTF-8".into())
}

/// Refuse a write whose path traverses a symlink anywhere below `root`.
///
/// Every check above is lexical: it proves the *string* names a file this
/// agent owns. A symlink at that name — or at any directory on the way to it
/// — makes the string say one thing and the write land somewhere else, since
/// `fs::write` follows links. That is the whole ownership restriction, and it
/// would also carry the approval-gate bypass to a file outside `personas/`
/// entirely. Cheap to close, and nothing legitimate creates one: every writer
/// of this tree writes regular files.
pub(crate) fn reject_symlinked_path(path: &Path, root: &Path) -> ToolResult<()> {
    let mut cursor = path.to_path_buf();
    while cursor.starts_with(root) {
        match std::fs::symlink_metadata(&cursor) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(ToolError::InvalidParams(format!(
                    "{} is a symlink; files here must be regular files inside \
                     this agent's own persona directory",
                    cursor.display()
                )));
            }
            // Absent is fine — these files are seeded on first read, and a
            // missing component cannot be a link.
            _ => {}
        }
        if !cursor.pop() {
            break;
        }
    }
    Ok(())
}

/// Enforce the size cap on an existing file so a corrupted multi-MiB blob
/// can't be slurped into memory just to replace one byte. A path that will
/// not `stat` passes — usually because it does not exist yet, and otherwise
/// because the read or write that follows is the one that should report the
/// real error rather than have it arrive dressed as a size complaint. The
/// cap on incoming *content* is the caller's to apply, since only it knows
/// the body.
///
/// Applied by the callers that actually read or write bytes, never by path
/// resolution: a file that somehow crossed the cap must still be
/// *removable*, and `MemoryDelete` reads nothing. Capping the resolver
/// instead would leave an oversized memory permanently stuck — unreadable,
/// unwritable, undeletable — and if it were `MEMORY.md`, injected at that
/// size into every prompt until someone opened a shell.
pub(crate) fn reject_if_oversized(path: &Path) -> ToolResult<()> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_MANAGED_FILE_BYTES => Err(ToolError::Execution(format!(
            "{} is {} bytes (> {} MiB cap); refusing to write",
            path.display(),
            meta.len(),
            MAX_MANAGED_FILE_BYTES >> 20
        ))),
        Ok(_) | Err(_) => Ok(()),
    }
}

/// Reject an incoming body that would exceed the cap, before it is written.
pub(crate) fn reject_oversized_content(len: usize, path: &Path) -> ToolResult<()> {
    if len as u64 > MAX_MANAGED_FILE_BYTES {
        return Err(ToolError::Execution(format!(
            "{} would be {len} bytes (> {} MiB cap); refusing to write",
            path.display(),
            MAX_MANAGED_FILE_BYTES >> 20
        )));
    }
    Ok(())
}

/// Stage and commit one audited change into `personas/`.
///
/// `Ok(None)` means there was nothing to record (a rewrite with identical
/// bytes) — `git commit` exits non-zero on an empty commit, and reporting
/// that as a failure would put a warning on every no-op save. `Err` carries
/// a reason for the caller's `commit_warning` line; the mutation itself is
/// already on disk either way.
pub(crate) async fn commit_change(
    repo: &Path,
    target: &ManagedTarget,
    kind: ChangeKind,
    tool: &str,
    session_id: &str,
) -> Result<Option<String>, String> {
    // `git` holds `.git/index.lock` across add + commit and a loser exits
    // rather than waiting, which the dream fan-out makes routine. Shared
    // with `personas/`'s other writer — the baseline commits in
    // `baybo-workspace` — so the two cannot race each other either.
    let _guard = baybo_workspace::personas_git_lock().lock().await;
    let file_name = target.rel_path.as_str();
    if !is_on_branch(repo).await? {
        return Err(format!(
            "HEAD is detached; check out a branch in {PERSONAS_DIR}/ first"
        ));
    }

    // `-A` so a removal is staged as unambiguously as a modification.
    run_git(repo, &[], &["add", "-A", "--", file_name]).await?;

    if !has_staged_change(repo, file_name).await? {
        return Ok(None);
    }

    let verb = kind.verb();
    let commit_msg = format!(
        "{PERSONAS_DIR}: {verb} {file_name}\n\n\
         Tool: {tool}\n\
         Session: {session_id}\n",
    );
    run_git(
        repo,
        &[
            "-c",
            &format!("user.name={GIT_AUTHOR_NAME}"),
            "-c",
            &format!("user.email={GIT_AUTHOR_EMAIL}"),
        ],
        &[
            "commit",
            "--no-verify",
            "--quiet",
            "-m",
            commit_msg.as_str(),
            "--",
            file_name,
        ],
    )
    .await?;

    let sha = run_git(repo, &[], &["rev-parse", "--short", "HEAD"]).await?;
    Ok(Some(sha.trim().to_string()))
}

/// Append the audit outcome to a tool's own output text.
pub(crate) fn append_audit_line(text: &mut String, outcome: Result<Option<String>, String>) {
    match outcome {
        Ok(Some(sha)) => text.push_str(&format!("\ncommitted to {PERSONAS_DIR}/ ({sha})")),
        Ok(None) => {}
        Err(reason) => text.push_str(&format!("\n{PERSONAS_DIR}/ commit_warning: {reason}")),
    }
}

/// Whether the index holds a change for `file_name` relative to HEAD.
///
/// A repo with no commits at all has no HEAD to diff against, and there
/// everything staged is a change by definition.
async fn has_staged_change(repo: &Path, file_name: &str) -> Result<bool, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff", "--cached", "--quiet", "--", file_name])
        .output()
        .await
        .map_err(|e| format!("spawn git diff: {e}"))?;
    // Exit 0 = no staged difference, 1 = differences. Anything else (e.g.
    // the no-HEAD case) is not a reliable "nothing to do" signal, so fall
    // through to the commit and let it report.
    Ok(!output.status.success())
}

async fn is_on_branch(repo: &Path) -> Result<bool, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .output()
        .await
        .map_err(|e| format!("spawn git symbolic-ref: {e}"))?;
    Ok(output.status.success())
}

async fn run_git(
    repo: &Path,
    config_overrides: &[&str],
    subcommand: &[&str],
) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo);
    for tok in config_overrides {
        cmd.arg(tok);
    }
    for tok in subcommand {
        cmd.arg(tok);
    }
    let output = cmd.output().await.map_err(|e| format!("spawn git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_workspace::IdentityKind;

    fn roots(root: &Path) -> ManagedRoots {
        ManagedRoots::new(&WorkspacePaths::new(root.to_path_buf()))
    }

    fn agent(id: &str) -> AgentProfileId {
        AgentProfileId::parse(id).expect("valid id")
    }

    #[test]
    fn a_memory_file_resolves_without_a_filename_allowlist() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let r = roots(tmp.path());
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());

        let target = r
            .memory_target(
                &paths.persona_memory_dir("baybo").join("anything-at-all.md"),
                &agent("baybo"),
            )
            .expect("resolve")
            .expect("is a memory target");
        assert_eq!(target.rel_path, "baybo/memory/anything-at-all.md");
    }

    #[test]
    fn another_agents_memory_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let r = roots(tmp.path());
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());

        // The same rule the identity files carry: an agent writes its own.
        let theirs = paths.persona_memory_dir("01JSCOUT").join("fact.md");
        assert!(r.memory_target(&theirs, &agent("baybo")).is_err());
        // …but the shape still bypasses the gate, because the declaration
        // has no call context; execute is what refuses it.
        assert!(r.bypasses_approval(&theirs));
    }

    #[test]
    fn the_shared_human_profile_is_writable_by_any_agent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let r = roots(tmp.path());
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());
        let shared = paths.shared_user_file();

        assert!(r.bypasses_approval(&shared));
        for who in ["baybo", "01JSCOUT"] {
            let target = r
                .identity_target(&shared, &agent(who))
                .expect("resolve")
                .expect("the shared profile belongs to no agent");
            assert_eq!(target.rel_path, "USER.md", "for {who}");
        }
    }

    #[test]
    fn another_agents_own_user_notes_are_still_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let r = roots(tmp.path());
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());

        // Sharing the *shared* profile does not share the per-agent one:
        // those notes stay each agent's own.
        let theirs = paths.persona_identity_file("01JSCOUT", IdentityKind::User);
        assert!(r.identity_target(&theirs, &agent("baybo")).is_err());
    }

    #[test]
    fn the_repos_own_git_metadata_is_never_writable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let r = roots(tmp.path());
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());

        // `personas/` IS a git repo, so `.git/` sits inside the tree it
        // audits. Writable, `core.fsmonitor` in `.git/config` would execute
        // on the audit commit's own `git add`.
        for inside_git in [
            paths.personas_dir().join(".git").join("config"),
            paths.persona_dir("baybo").join(".git").join("HEAD"),
            paths
                .persona_memory_dir("baybo")
                .join(".git")
                .join("config"),
        ] {
            assert!(
                !r.bypasses_approval(&inside_git),
                "{}",
                inside_git.display()
            );
            assert!(
                r.memory_target(&inside_git, &agent("baybo"))
                    .expect("resolve")
                    .is_none(),
                "{}",
                inside_git.display()
            );
        }
    }

    #[test]
    fn a_walk_out_of_the_tree_earns_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let r = roots(tmp.path());
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());

        let escape = paths.persona_memory_dir("baybo").join("..").join("SOUL.md");
        assert!(!r.bypasses_approval(&escape));
        assert!(
            r.memory_target(&escape, &agent("baybo"))
                .expect("resolve")
                .is_none()
        );
    }

    #[test]
    fn a_symlink_cannot_lend_its_location_to_a_target_outside_the_tree() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());
        let r = roots(tmp.path());
        let me = agent("baybo");
        std::fs::create_dir_all(paths.persona_memory_dir("baybo")).expect("mkdir");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::fs::write(outside.join("secret.txt"), "not a memory").expect("write");

        let linked_file = paths.persona_memory_dir("baybo").join("note.md");
        std::os::unix::fs::symlink(outside.join("secret.txt"), &linked_file).expect("symlink");
        assert!(r.memory_target(&linked_file, &me).is_err());

        // A symlinked *directory* on the way to a file that does not exist
        // yet — the case a canonicalize-the-file check would miss.
        let linked_dir = paths.persona_memory_dir("baybo").join("sys");
        std::os::unix::fs::symlink(&outside, &linked_dir).expect("symlink");
        assert!(r.memory_target(&linked_dir.join("fresh.md"), &me).is_err());

        // A genuine file in the tree is unaffected.
        let real = paths.persona_memory_dir("baybo").join("cat-name.md");
        assert!(r.memory_target(&real, &me).expect("resolve").is_some());
    }

    #[test]
    fn the_memory_dir_itself_is_not_a_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let r = roots(tmp.path());
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());

        assert!(
            r.memory_target(&paths.persona_memory_dir("baybo"), &agent("baybo"))
                .expect("resolve")
                .is_none()
        );
    }

    #[test]
    fn a_lookalike_outside_the_workspace_earns_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let r = roots(tmp.path());
        let decoy = Path::new("/etc/personas/baybo/memory/fact.md");

        assert!(!r.bypasses_approval(decoy));
        assert!(
            r.memory_target(decoy, &agent("baybo"))
                .expect("resolve")
                .is_none()
        );
    }

    /// The dream pass fans out to one fire per agent, all of them writing
    /// into the one `personas/` repo at once — so concurrent audit commits
    /// are routine, not exotic. `git` takes `.git/index.lock` for the whole
    /// of `add` + `commit`, and a loser does not retry: it would report a
    /// `commit_warning` and leave the change on disk but unrecorded.
    #[tokio::test]
    async fn concurrent_audit_commits_all_land() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());
        let repo = paths.personas_dir();
        let me = agent("baybo");
        let memory = paths.persona_memory_dir("baybo");
        std::fs::create_dir_all(&memory).expect("mkdir");
        for args in [
            vec!["init", "--quiet", "-b", "main"],
            vec!["commit", "--quiet", "--allow-empty", "-m", "root"],
        ] {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=T", "-c", "user.email=t@l"])
                .args(&args)
                .status()
                .expect("git")
                .success();
            assert!(ok, "git {args:?}");
        }

        let roots = std::sync::Arc::new(ManagedRoots::new(&paths));
        let mut handles = Vec::new();
        for i in 0..4 {
            let roots = std::sync::Arc::clone(&roots);
            let path = memory.join(format!("fact-{i}.md"));
            std::fs::write(&path, format!("fact {i}")).expect("write");
            let me = me.clone();
            let repo = repo.clone();
            handles.push(tokio::spawn(async move {
                let target = roots
                    .memory_target(&path, &me)
                    .expect("resolve")
                    .expect("memory target");
                commit_change(&repo, &target, ChangeKind::Update, "Write", "sess").await
            }));
        }

        for (i, handle) in handles.into_iter().enumerate() {
            let outcome = handle.await.expect("join");
            assert!(
                matches!(outcome, Ok(Some(_))),
                "commit {i} must land, got: {outcome:?}"
            );
        }
    }

    #[test]
    fn scratch_roots_bypass_approval_but_are_not_audited() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let r = roots(tmp.path());
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());

        let scratch = paths.work_dir().join("scratch.txt");
        assert!(r.bypasses_approval(&scratch));
        assert!(
            r.audit_target(&scratch, &agent("baybo"))
                .expect("resolve")
                .is_none()
        );
    }
}
