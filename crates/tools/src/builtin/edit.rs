//! `Edit` — targeted string replacement inside an existing file.
//!
//! Edits that target an **identity file** pick up three extra guards
//! before the write. Two locations qualify, because an agent's soul is
//! wherever that agent's soul lives:
//!
//! - `<workspace>/profile/{SOUL,USER,IDENTITY}.md` — the shared
//!   three-slot store, and the built-in agent's own persona.
//! - `<workspace>/personas/<agent_id>/SOUL.md` — one custom agent's
//!   soul. Only `SOUL.md`, and only directly under the agent's own
//!   directory: a persona's `skills/` tree is skill content, not
//!   identity.
//!
//! The guards:
//!
//! - **Allowlist**: nothing else in either location may be touched.
//!   They are declarative stores, not freeform scratch space.
//! - **Size cap**: identity files are system-prompt-bound (~kB). A
//!   multi-MiB file is corruption or symlink shenanigans — fail before
//!   we slurp it.
//! - **Audit commit**: after a successful write, the change is staged
//!   and committed into the owning standalone git repo (`profile/` or
//!   `personas/`) with a fixed `Baybo <baybo@local>` author, so the user
//!   can later see what the agent rewrote and revert with `git`.
//!   `--no-verify` is intentional: both are Baybo-managed audit history,
//!   not hand-curated repos where pre-commit hooks would be authored. A
//!   commit failure (detached HEAD, missing git, etc.) leaves the file
//!   write in place and surfaces a warning in the tool output.
//!
//! This is what keeps the system prompt's own instruction honest: it
//! names each identity file's absolute path and tells the model to
//! `Edit` it, so a self-edit must not be a per-turn approval prompt with
//! no audit trail — for *every* agent, not just the built-in.
//!
//! Edits under `<workspace>/work/` and `<workspace>/skills/` also skip
//! the approval gate (matching the `Write` tool's `work/` bypass), but
//! without the identity-file allowlist, size cap, or audit commit —
//! those roots are agent scratch / managed skill content, not an
//! identity store.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use baybo_workspace::{IdentityKind, WorkspacePaths, absolutise};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use super::paths::require_absolute;
use crate::{ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

const GIT_AUTHOR_NAME: &str = "Baybo";
const GIT_AUTHOR_EMAIL: &str = "baybo@local";
const MAX_IDENTITY_BYTES: u64 = 1 << 20;

/// An identity-file edit, resolved to the git repo that audits it and the
/// path to stage inside that repo.
struct IdentityTarget {
    repo: PathBuf,
    /// Repo-relative, so `git add --` stages exactly this file:
    /// `SOUL.md` under `profile/`, `<agent_id>/SOUL.md` under `personas/`.
    rel_path: String,
    /// Dir name for the tool's own output line (`profile` / `personas`).
    repo_label: &'static str,
}

pub struct EditTool {
    profile_dir: PathBuf,
    personas_dir: PathBuf,
    work_dir: PathBuf,
    skills_dir: PathBuf,
}

impl EditTool {
    pub fn new(workspace_paths: WorkspacePaths) -> Self {
        // Bake absolutised dirs so `starts_with` comparisons work even
        // when the workspace root is relative — the debug-build default
        // is `./.baybo`, and a relative prefix never matches an absolute
        // file path the LLM passes from the system prompt's
        // `<soul path="...">` wrapper.
        Self {
            profile_dir: absolutise(&workspace_paths.profile_dir()),
            personas_dir: absolutise(&workspace_paths.personas_dir()),
            work_dir: absolutise(&workspace_paths.work_dir()),
            skills_dir: absolutise(&workspace_paths.skills_dir()),
        }
    }

    /// True when the requested edit targets one of the three identity
    /// files inside the actual workspace `profile/` directory. Bound
    /// to the absolutised profile dir so a spoofed path like
    /// `/etc/profile/SOUL.md` cannot satisfy the check.
    fn is_profile_target(&self, file_path: &Path) -> bool {
        if !file_path.starts_with(&self.profile_dir) {
            return false;
        }
        let Some(name) = file_path.file_name().and_then(|f| f.to_str()) else {
            return false;
        };
        IdentityKind::all().iter().any(|k| k.file_name() == name)
    }

    /// True for exactly `<workspace>/personas/<agent_id>/SOUL.md` — one
    /// custom agent's soul.
    ///
    /// The grandparent check is what keeps a persona's `skills/` tree out:
    /// `personas/<id>/skills/x/SOUL.md` is skill content that happens to be
    /// named like a soul, and must not inherit the identity treatment.
    fn is_persona_soul(&self, file_path: &Path) -> bool {
        if !file_path.starts_with(&self.personas_dir) {
            return false;
        }
        if file_path.file_name().and_then(|f| f.to_str()) != Some(IdentityKind::Soul.file_name()) {
            return false;
        }
        file_path
            .parent()
            .and_then(Path::parent)
            .is_some_and(|grandparent| grandparent == self.personas_dir)
    }

    /// Whether this edit gets the identity treatment (approval bypass, and
    /// — once resolved by [`Self::identity_target`] — the size cap and audit
    /// commit).
    fn is_identity_target(&self, file_path: &Path) -> bool {
        self.is_profile_target(file_path) || self.is_persona_soul(file_path)
    }

    /// Resolve an identity edit to the repo that audits it, rejecting a path
    /// inside one of the two stores that is not an allowed file.
    /// `None` when the path is in neither store.
    fn identity_target(&self, file_path: &Path) -> crate::Result<Option<IdentityTarget>> {
        if file_path.starts_with(&self.profile_dir) {
            let name = check_profile_target(file_path)?;
            return Ok(Some(IdentityTarget {
                repo: self.profile_dir.clone(),
                rel_path: name.to_owned(),
                repo_label: baybo_workspace::paths::PROFILE_DIR,
            }));
        }
        if file_path.starts_with(&self.personas_dir) {
            let rel_path = check_persona_soul_target(file_path, &self.personas_dir)?;
            return Ok(Some(IdentityTarget {
                repo: self.personas_dir.clone(),
                rel_path,
                repo_label: baybo_workspace::paths::PERSONAS_DIR,
            }));
        }
        Ok(None)
    }

    fn is_inside_work_dir(&self, file_path: &Path) -> bool {
        file_path.is_absolute() && file_path.starts_with(&self.work_dir)
    }

    fn is_inside_skills_dir(&self, file_path: &Path) -> bool {
        file_path.is_absolute() && file_path.starts_with(&self.skills_dir)
    }
}

#[derive(Debug, Deserialize)]
struct Params {
    file_path: PathBuf,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> String {
        "Perform targeted string replacement inside a file. Always use \
         this instead of Bash commands like sed or awk for editing files. \
         Replace `old_string` with `new_string`; when `replace_all` is \
         false (default), `old_string` must appear exactly once — otherwise \
         the tool fails without touching the file. Provide enough surrounding \
         context in `old_string` to ensure a unique match.\n\n\
         READ FIRST: you must Read the file before editing it. If it changed \
         on disk since your last Read, Read it again — the edit is rejected \
         until your view is current.\n\n\
         PATHS: `file_path` MUST be an absolute filesystem path. Relative \
         paths are rejected."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path":   { "type": "string" },
                "old_string":  { "type": "string" },
                "new_string":  { "type": "string" },
                "replace_all": { "type": "boolean", "default": false }
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        params
            .get("file_path")
            .and_then(Value::as_str)
            .map(|s| crate::progress::preview_path(Path::new(s)))
    }

    fn accessed_resources(&self, params: &Value) -> Vec<ResourceAccess> {
        let Some(s) = params.get("file_path").and_then(|v| v.as_str()) else {
            return Vec::new();
        };
        let path = PathBuf::from(s);
        // Bypass the approval gate for edits inside the workspace's
        // managed roots: identity files under `profile/` (audit trail
        // is the per-edit git commit, not a user prompt), and any path
        // under `work/` or `skills/` (agent scratch / managed skill
        // content). Anything else still goes through the gate.
        if self.is_identity_target(&path)
            || self.is_inside_work_dir(&path)
            || self.is_inside_skills_dir(&path)
        {
            return vec![ResourceAccess::ReadFile { path }];
        }
        vec![
            ResourceAccess::ReadFile { path: path.clone() },
            ResourceAccess::WriteFile { path },
        ]
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        require_absolute(&p.file_path, "Edit", "file_path")?;

        if p.old_string == p.new_string {
            return Err(ToolError::InvalidParams(
                "old_string and new_string are identical".into(),
            ));
        }

        let identity_target = self.identity_target(Path::new(&p.file_path))?;

        // Read-before-write contract: the file must have been read in this
        // session and be unchanged since. `stat` it for the current
        // fingerprint; a file that fails to stat (missing) skips the check
        // and is reported by the read below.
        if let Some(tracker) = &ctx.read_tracker
            && let Ok(meta) = tokio::fs::metadata(&p.file_path).await
            && let Some(reason) = tracker
                .check(&p.file_path, crate::FileFingerprint::from_metadata(&meta))
                .rejection(&p.file_path, "editing it")
        {
            return Err(ToolError::Execution(reason));
        }

        let contents = tokio::fs::read_to_string(&p.file_path)
            .await
            .map_err(|e| ToolError::Execution(format!("read {}: {e}", p.file_path.display())))?;

        let matches = contents.matches(&p.old_string).count();
        if matches == 0 {
            return Err(ToolError::Execution("old_string not found in file".into()));
        }
        if !p.replace_all && matches > 1 {
            return Err(ToolError::Execution(format!(
                "old_string matches {matches} times; set replace_all=true or add more context"
            )));
        }

        let updated = if p.replace_all {
            contents.replace(&p.old_string, &p.new_string)
        } else {
            contents.replacen(&p.old_string, &p.new_string, 1)
        };

        tokio::fs::write(&p.file_path, &updated)
            .await
            .map_err(|e| ToolError::Execution(format!("write {}: {e}", p.file_path.display())))?;

        // Re-anchor the read baseline to the file we just wrote so a chained
        // Edit on the same file does not demand an intervening re-read.
        if let Some(tracker) = &ctx.read_tracker {
            tracker.record_write_from_disk(&p.file_path);
        }

        let replaced = if p.replace_all { matches } else { 1 };
        let mut text = format!(
            "replaced {replaced} occurrence(s) in {}",
            p.file_path.display()
        );

        if let Some(target) = identity_target {
            let label = target.repo_label;
            match commit_identity_change(&target, ctx.session_id.as_str()).await {
                Ok(sha) => text.push_str(&format!("\ncommitted to {label}/ ({sha})")),
                Err(reason) => text.push_str(&format!("\n{label}/ commit_warning: {reason}")),
            }
            text.push_str(
                "\nnote: the in-memory system prompt picks up this change on the next compaction or new session",
            );
        }

        Ok(ToolOutput::Text(text))
    }
}

/// Resolve a `profile/`-relative edit to one of the three identity
/// filenames, or reject with `InvalidParams`. Also enforces the size
/// cap on the existing file so a corrupted multi-MiB blob can't be
/// slurped into memory just to replace one byte.
fn check_profile_target(path: &Path) -> crate::Result<&'static str> {
    let raw_name = path.file_name().and_then(|f| f.to_str()).ok_or_else(|| {
        ToolError::InvalidParams(format!(
            "edits under profile/ require a filename; got {}",
            path.display()
        ))
    })?;

    let canonical = IdentityKind::all()
        .into_iter()
        .map(|k| k.file_name())
        .find(|name| *name == raw_name)
        .ok_or_else(|| {
            ToolError::InvalidParams(format!(
                "edits under profile/ are restricted to SOUL.md, USER.md, IDENTITY.md; got {raw_name}"
            ))
        })?;

    reject_if_oversized(path)?;
    Ok(canonical)
}

/// Resolve a `personas/`-relative edit to `<agent_id>/SOUL.md`, or reject
/// with `InvalidParams`. A persona directory holds one identity file; its
/// `skills/` tree is skill content and is not editable through this path.
fn check_persona_soul_target(path: &Path, personas_dir: &Path) -> crate::Result<String> {
    let rel = path.strip_prefix(personas_dir).map_err(|_| {
        ToolError::InvalidParams(format!("{} is not under personas/", path.display()))
    })?;
    let components: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    // Exactly `<agent_id>/SOUL.md` — a longer path is either the persona's
    // `skills/` tree or a `..` walk, and neither is an identity file.
    if components.len() != 2 || components[1] != IdentityKind::Soul.file_name() {
        return Err(ToolError::InvalidParams(format!(
            "edits under personas/ are restricted to <agent_id>/{}; got {}",
            IdentityKind::Soul.file_name(),
            rel.display()
        )));
    }
    reject_if_oversized(path)?;
    Ok(format!("{}/{}", components[0], components[1]))
}

/// Enforce the size cap on an existing identity file so a corrupted
/// multi-MiB blob can't be slurped into memory just to replace one byte.
fn reject_if_oversized(path: &Path) -> crate::Result<()> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_IDENTITY_BYTES => Err(ToolError::Execution(format!(
            "identity file {} is {} bytes (> {} MiB cap); refusing to edit",
            path.display(),
            meta.len(),
            MAX_IDENTITY_BYTES >> 20
        ))),
        Ok(_) | Err(_) => Ok(()),
    }
}

async fn commit_identity_change(
    target: &IdentityTarget,
    session_id: &str,
) -> Result<String, String> {
    let IdentityTarget {
        repo,
        rel_path,
        repo_label,
    } = target;
    let repo = repo.as_path();
    let file_name = rel_path.as_str();
    if !is_on_branch(repo).await? {
        return Err(format!(
            "HEAD is detached; check out a branch in {repo_label}/ first"
        ));
    }

    run_git(repo, &[], &["add", "--", file_name]).await?;

    let commit_msg = format!(
        "{repo_label}: update {file_name}\n\n\
         Tool: Edit\n\
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
    Ok(sha.trim().to_string())
}

async fn is_on_branch(profile_dir: &Path) -> Result<bool, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(profile_dir)
        .args(["symbolic-ref", "--quiet", "HEAD"])
        .output()
        .await
        .map_err(|e| format!("spawn git symbolic-ref: {e}"))?;
    Ok(output.status.success())
}

async fn run_git(
    profile_dir: &Path,
    config_overrides: &[&str],
    subcommand: &[&str],
) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(profile_dir);
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
    use baybo_model::{ChannelType, User};
    use baybo_workspace::WorkspacePaths;
    use std::time::Duration;

    fn ctx() -> ToolContext {
        ToolContext {
            session_id: "t".into(),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            timeout: Duration::from_secs(5),
            workspace_root: std::path::PathBuf::from("/tmp"),
            workspace_paths: WorkspacePaths::new("/tmp"),
            ..ToolContext::for_test()
        }
    }

    fn ctx_with_paths(paths: WorkspacePaths) -> ToolContext {
        ToolContext {
            session_id: "sess-test".into(),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            timeout: Duration::from_secs(10),
            workspace_root: paths.work_dir(),
            workspace_paths: paths,
            ..ToolContext::for_test()
        }
    }

    async fn run_git_quiet(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .await
            .expect("git");
        assert!(status.success(), "git {args:?} failed");
    }

    async fn make_profile_workspace() -> (tempfile::TempDir, WorkspacePaths) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());
        let profile = paths.profile_dir();
        tokio::fs::create_dir_all(&profile).await.unwrap();
        run_git_quiet(&profile, &["init", "--quiet", "-b", "main"]).await;
        for kind in IdentityKind::all() {
            tokio::fs::write(paths.identity_file(kind), "## seed\n")
                .await
                .unwrap();
        }
        run_git_quiet(
            &profile,
            &[
                "-c",
                "user.name=seed",
                "-c",
                "user.email=seed@local",
                "add",
                ".",
            ],
        )
        .await;
        run_git_quiet(
            &profile,
            &[
                "-c",
                "user.name=seed",
                "-c",
                "user.email=seed@local",
                "commit",
                "--no-verify",
                "--quiet",
                "-m",
                "seed",
            ],
        )
        .await;
        (tmp, paths)
    }

    async fn make_fresh_profile_workspace() -> (tempfile::TempDir, WorkspacePaths) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());
        let profile = paths.profile_dir();
        tokio::fs::create_dir_all(&profile).await.unwrap();
        run_git_quiet(&profile, &["init", "--quiet", "-b", "main"]).await;
        for kind in IdentityKind::all() {
            tokio::fs::write(paths.identity_file(kind), "alpha bravo charlie\n")
                .await
                .unwrap();
        }
        (tmp, paths)
    }

    fn tool() -> EditTool {
        EditTool::new(WorkspacePaths::new("/tmp"))
    }

    fn tool_with(paths: WorkspacePaths) -> EditTool {
        EditTool::new(paths)
    }

    #[tokio::test]
    async fn single_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        tokio::fs::write(&p, "alpha beta gamma").await.unwrap();
        tool()
            .execute(
                json!({ "file_path": p, "old_string": "beta", "new_string": "BETA" }),
                &ctx(),
            )
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&p).await.unwrap(),
            "alpha BETA gamma"
        );
    }

    #[tokio::test]
    async fn rejects_ambiguous_match() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        tokio::fs::write(&p, "x x").await.unwrap();
        let err = tool()
            .execute(
                json!({ "file_path": p, "old_string": "x", "new_string": "y" }),
                &ctx(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("matches 2 times"));
    }

    #[tokio::test]
    async fn rejects_relative_path() {
        let err = tool()
            .execute(
                json!({ "file_path": "rel.txt", "old_string": "a", "new_string": "b" }),
                &ctx(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParams(ref m) if m.contains("absolute")),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn replace_all() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        tokio::fs::write(&p, "x x x").await.unwrap();
        tool()
            .execute(
                json!({
                    "file_path": p,
                    "old_string": "x",
                    "new_string": "y",
                    "replace_all": true
                }),
                &ctx(),
            )
            .await
            .unwrap();
        assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "y y y");
    }

    fn ctx_with_tracker(tracker: crate::ReadTracker) -> ToolContext {
        ToolContext {
            read_tracker: Some(tracker),
            ..ctx()
        }
    }

    #[tokio::test]
    async fn rejects_edit_without_prior_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        tokio::fs::write(&p, "alpha beta gamma").await.unwrap();
        let err = tool()
            .execute(
                json!({ "file_path": p, "old_string": "beta", "new_string": "BETA" }),
                &ctx_with_tracker(crate::ReadTracker::default()),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Execution(ref m) if m.contains("has not been read")),
            "got: {err:?}"
        );
        // File untouched on a contract rejection.
        assert_eq!(
            tokio::fs::read_to_string(&p).await.unwrap(),
            "alpha beta gamma"
        );
    }

    #[tokio::test]
    async fn allows_edit_after_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        tokio::fs::write(&p, "alpha beta gamma").await.unwrap();
        let tracker = crate::ReadTracker::default();
        tracker.record_write_from_disk(&p);
        tool()
            .execute(
                json!({ "file_path": p, "old_string": "beta", "new_string": "BETA" }),
                &ctx_with_tracker(tracker),
            )
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&p).await.unwrap(),
            "alpha BETA gamma"
        );
    }

    #[tokio::test]
    async fn rejects_edit_when_file_changed_since_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        tokio::fs::write(&p, "alpha beta gamma").await.unwrap();
        let tracker = crate::ReadTracker::default();
        tracker.record_write_from_disk(&p);
        // External modification of a different length flips the fingerprint
        // regardless of mtime resolution.
        tokio::fs::write(&p, "alpha beta gamma delta epsilon")
            .await
            .unwrap();
        let err = tool()
            .execute(
                json!({ "file_path": p, "old_string": "beta", "new_string": "BETA" }),
                &ctx_with_tracker(tracker),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Execution(ref m) if m.contains("changed on disk")),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn chained_edits_need_only_one_read() {
        // A successful Edit re-anchors the baseline, so the second Edit on the
        // same file succeeds without an intervening Read.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        tokio::fs::write(&p, "a b c").await.unwrap();
        let tracker = crate::ReadTracker::default();
        tracker.record_write_from_disk(&p);
        let ctx = ctx_with_tracker(tracker);
        tool()
            .execute(
                json!({ "file_path": p, "old_string": "a", "new_string": "A" }),
                &ctx,
            )
            .await
            .unwrap();
        tool()
            .execute(
                json!({ "file_path": p, "old_string": "b", "new_string": "B" }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "A B c");
    }

    #[tokio::test]
    async fn profile_edit_commits_with_baybo_author() {
        let (_tmp, paths) = make_profile_workspace().await;
        let target = paths.identity_file(IdentityKind::Soul);
        tokio::fs::write(&target, "alpha bravo charlie\n")
            .await
            .unwrap();
        run_git_quiet(
            &paths.profile_dir(),
            &[
                "-c",
                "user.name=seed",
                "-c",
                "user.email=seed@local",
                "commit",
                "--no-verify",
                "--quiet",
                "-am",
                "set body",
            ],
        )
        .await;

        let ctx = ctx_with_paths(paths.clone());
        let out = tool_with(paths.clone())
            .execute(
                json!({
                    "file_path": target,
                    "old_string": "bravo",
                    "new_string": "BRAVO",
                }),
                &ctx,
            )
            .await
            .expect("execute");

        let body = tokio::fs::read_to_string(&paths.identity_file(IdentityKind::Soul))
            .await
            .unwrap();
        assert_eq!(body, "alpha BRAVO charlie\n");

        let ToolOutput::Text(text) = out else {
            panic!("expected Text output")
        };
        assert!(text.contains("committed to profile/"), "{text}");
        assert!(text.contains("next compaction or new session"), "{text}");

        let log = Command::new("git")
            .arg("-C")
            .arg(paths.profile_dir())
            .args(["log", "-1", "--pretty=%an <%ae>%n%B"])
            .output()
            .await
            .unwrap();
        let log = String::from_utf8_lossy(&log.stdout);
        assert!(log.contains("Baybo <baybo@local>"), "{log}");
        assert!(log.contains("Tool: Edit"), "{log}");
        assert!(log.contains("Session: sess-test"), "{log}");
    }

    /// A custom agent's soul lives under `personas/<id>/`, not `profile/`.
    /// The self-edit the system prompt instructs must get the same treatment
    /// there — no approval prompt, and an audit commit — or the instruction
    /// is only honest for the built-in agent.
    async fn make_persona_workspace(agent_id: &str) -> (tempfile::TempDir, WorkspacePaths) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());
        let personas = paths.personas_dir();
        tokio::fs::create_dir_all(paths.persona_skills_dir(agent_id))
            .await
            .unwrap();
        tokio::fs::write(paths.persona_soul_file(agent_id), "## seed\nalpha\n")
            .await
            .unwrap();
        run_git_quiet(&personas, &["init", "--quiet", "-b", "main"]).await;
        for args in [
            vec!["add", "."],
            vec!["commit", "--no-verify", "--quiet", "-m", "seed"],
        ] {
            let mut full = vec!["-c", "user.name=seed", "-c", "user.email=seed@local"];
            full.extend(args);
            run_git_quiet(&personas, &full).await;
        }
        (tmp, paths)
    }

    #[tokio::test]
    async fn persona_soul_edit_skips_approval_and_commits_to_personas() {
        let agent = "01JAGENT";
        let (_tmp, paths) = make_persona_workspace(agent).await;
        let target = paths.persona_soul_file(agent);

        // Approval gate: an identity edit declares only a read, so the
        // agent is not prompted every time it updates its own persona.
        let declared = tool_with(paths.clone()).accessed_resources(&json!({ "file_path": target }));
        assert_eq!(
            declared.len(),
            1,
            "persona soul must bypass the write gate: {declared:?}"
        );
        assert!(matches!(declared[0], ResourceAccess::ReadFile { .. }));

        let ctx = ctx_with_paths(paths.clone());
        let out = tool_with(paths.clone())
            .execute(
                json!({
                    "file_path": target,
                    "old_string": "alpha",
                    "new_string": "bravo",
                }),
                &ctx,
            )
            .await
            .unwrap();
        let ToolOutput::Text(text) = out else {
            panic!("expected text output")
        };
        assert!(text.contains("committed to personas/"), "{text}");

        // The commit landed in the personas/ repo and names the agent's own
        // file, so `git log` can show what the agent rewrote.
        let log = Command::new("git")
            .arg("-C")
            .arg(paths.personas_dir())
            .args(["log", "-1", "--pretty=%an <%ae>%n%s%n%b", "--name-only"])
            .output()
            .await
            .unwrap();
        let log = String::from_utf8_lossy(&log.stdout);
        assert!(log.contains("Baybo <baybo@local>"), "{log}");
        assert!(
            log.contains(&format!("personas: update {agent}/SOUL.md")),
            "{log}"
        );
        assert!(log.contains(&format!("{agent}/SOUL.md")), "{log}");
        assert_eq!(
            tokio::fs::read_to_string(&target).await.unwrap(),
            "## seed\nbravo\n"
        );
    }

    #[tokio::test]
    async fn a_personas_path_that_is_not_a_soul_gets_no_identity_treatment() {
        let agent = "01JAGENT";
        let (_tmp, paths) = make_persona_workspace(agent).await;

        // A skill inside the persona dir is skill content, not identity:
        // it keeps the write gate and takes no audit commit. Named SOUL.md
        // deliberately — the guard is the location, not the filename.
        let skill_dir = paths.persona_skills_dir(agent).join("deploy");
        tokio::fs::create_dir_all(&skill_dir).await.unwrap();
        let decoy = skill_dir.join("SOUL.md");
        tokio::fs::write(&decoy, "alpha\n").await.unwrap();

        let declared = tool_with(paths.clone()).accessed_resources(&json!({ "file_path": decoy }));
        assert!(
            declared.len() > 1,
            "a persona skill file must still go through the write gate: {declared:?}"
        );

        // And an edit under personas/ that is neither is refused outright.
        let stray = paths.persona_dir(agent).join("notes.md");
        tokio::fs::write(&stray, "alpha\n").await.unwrap();
        let ctx = ctx_with_paths(paths.clone());
        let err = tool_with(paths.clone())
            .execute(
                json!({
                    "file_path": stray,
                    "old_string": "alpha",
                    "new_string": "bravo",
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParams(ref m) if m.contains("SOUL.md")),
            "got: {err:?}"
        );
        assert_eq!(
            tokio::fs::read_to_string(&stray).await.unwrap(),
            "alpha\n",
            "file must not be modified on guard reject"
        );
    }

    #[tokio::test]
    async fn profile_edit_rejects_unknown_filename() {
        let (_tmp, paths) = make_profile_workspace().await;
        let stray = paths.profile_dir().join("notes.md");
        tokio::fs::write(&stray, "hello").await.unwrap();
        let ctx = ctx_with_paths(paths.clone());
        let err = tool_with(paths.clone())
            .execute(
                json!({
                    "file_path": stray,
                    "old_string": "hello",
                    "new_string": "world",
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParams(ref m) if m.contains("SOUL.md")),
            "got: {err:?}"
        );
        let body = tokio::fs::read_to_string(&stray).await.unwrap();
        assert_eq!(body, "hello", "file must not be modified on guard reject");
    }

    #[tokio::test]
    async fn profile_edit_rejects_oversized_file() {
        let (_tmp, paths) = make_profile_workspace().await;
        let target = paths.identity_file(IdentityKind::User);
        let big = "x".repeat((MAX_IDENTITY_BYTES + 1) as usize);
        tokio::fs::write(&target, &big).await.unwrap();
        let ctx = ctx_with_paths(paths.clone());
        let err = tool_with(paths.clone())
            .execute(
                json!({
                    "file_path": target,
                    "old_string": "x",
                    "new_string": "y",
                    "replace_all": true,
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Execution(ref m) if m.contains("MiB cap")),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn profile_edit_fresh_workspace_creates_commit() {
        let (_tmp, paths) = make_fresh_profile_workspace().await;
        let target = paths.identity_file(IdentityKind::Soul);
        let ctx = ctx_with_paths(paths.clone());
        let out = tool_with(paths.clone())
            .execute(
                json!({
                    "file_path": target,
                    "old_string": "bravo",
                    "new_string": "BRAVO",
                }),
                &ctx,
            )
            .await
            .expect("first call on fresh workspace");
        let ToolOutput::Text(text) = out else {
            panic!("expected Text")
        };
        assert!(
            text.contains("committed to profile/"),
            "first call must produce a real commit; got:\n{text}"
        );

        let log = Command::new("git")
            .arg("-C")
            .arg(paths.profile_dir())
            .args(["log", "--oneline"])
            .output()
            .await
            .unwrap();
        let log = String::from_utf8_lossy(&log.stdout);
        assert!(!log.is_empty(), "git log should show the new commit");
    }

    #[test]
    fn accessed_resources_drops_writefile_for_profile_targets() {
        let paths = WorkspacePaths::new("/var/baybo");
        let edit = EditTool::new(paths.clone());

        // Profile target → only ReadFile is declared; the WriteFile
        // declaration that would otherwise force an approval prompt
        // is omitted.
        let in_profile = paths.identity_file(IdentityKind::Soul);
        let resources =
            edit.accessed_resources(&json!({ "file_path": in_profile.to_string_lossy() }));
        assert_eq!(resources.len(), 1);
        assert!(matches!(resources[0], ResourceAccess::ReadFile { .. }));

        // Non-profile target → both Read and Write declared; gate still fires.
        let resources = edit.accessed_resources(&json!({ "file_path": "/tmp/random.txt" }));
        assert_eq!(resources.len(), 2);
        assert!(
            resources
                .iter()
                .any(|r| matches!(r, ResourceAccess::WriteFile { .. }))
        );
    }

    #[test]
    fn accessed_resources_drops_writefile_for_work_targets() {
        let paths = WorkspacePaths::new("/var/baybo");
        let edit = EditTool::new(paths.clone());

        let in_work = paths.work_dir().join("scratch/notes.txt");
        let resources = edit.accessed_resources(&json!({ "file_path": in_work.to_string_lossy() }));
        assert_eq!(resources.len(), 1, "{resources:?}");
        assert!(matches!(resources[0], ResourceAccess::ReadFile { .. }));
    }

    #[test]
    fn accessed_resources_drops_writefile_for_skills_targets() {
        let paths = WorkspacePaths::new("/var/baybo");
        let edit = EditTool::new(paths.clone());

        let in_skills = paths.skills_dir().join("my-skill/SKILL.md");
        let resources =
            edit.accessed_resources(&json!({ "file_path": in_skills.to_string_lossy() }));
        assert_eq!(resources.len(), 1, "{resources:?}");
        assert!(matches!(resources[0], ResourceAccess::ReadFile { .. }));
    }

    #[test]
    fn accessed_resources_bypass_works_with_relative_workspace_root() {
        // Regression: debug-build default is `./.baybo` — a relative
        // workspace root. profile_dir() returns the relative
        // `./.baybo/profile`, but the LLM passes file paths as absolute
        // (the system prompt wraps each identity file with the
        // absolutised on-disk path). Without absolutising the cached
        // profile dir, `starts_with` never matched and the bypass
        // silently failed in dev. Lock that in.
        let cwd = std::env::current_dir().expect("cwd");
        let edit = EditTool::new(WorkspacePaths::new("./.baybo"));

        let absolute_target = cwd.join(".baybo/profile/SOUL.md");
        let resources =
            edit.accessed_resources(&json!({ "file_path": absolute_target.to_string_lossy() }));
        assert_eq!(
            resources.len(),
            1,
            "relative workspace root must still produce a profile bypass: {resources:?}"
        );
        assert!(matches!(resources[0], ResourceAccess::ReadFile { .. }));
    }

    #[test]
    fn accessed_resources_does_not_bypass_spoofed_profile_path() {
        // Path looks like a profile edit (ends in /profile/SOUL.md) but
        // sits outside the actual workspace. Must NOT bypass approval —
        // otherwise the LLM could write anywhere matching that shape.
        let paths = WorkspacePaths::new("/var/baybo");
        let edit = EditTool::new(paths);
        let resources = edit.accessed_resources(&json!({ "file_path": "/etc/profile/SOUL.md" }));
        assert_eq!(resources.len(), 2);
        assert!(
            resources
                .iter()
                .any(|r| matches!(r, ResourceAccess::WriteFile { .. })),
            "spoofed profile-shaped path must still declare WriteFile",
        );
    }

    #[tokio::test]
    async fn profile_edit_detached_head_warns_in_output() {
        let (_tmp, paths) = make_profile_workspace().await;
        let head = Command::new("git")
            .arg("-C")
            .arg(paths.profile_dir())
            .args(["rev-parse", "HEAD"])
            .output()
            .await
            .unwrap();
        let sha = String::from_utf8_lossy(&head.stdout).trim().to_string();
        run_git_quiet(&paths.profile_dir(), &["checkout", "--quiet", &sha]).await;

        let target = paths.identity_file(IdentityKind::Soul);
        let ctx = ctx_with_paths(paths.clone());
        let out = tool_with(paths.clone())
            .execute(
                json!({
                    "file_path": target,
                    "old_string": "seed",
                    "new_string": "SEED",
                }),
                &ctx,
            )
            .await
            .expect("execute should still succeed with warning");
        let ToolOutput::Text(text) = out else {
            panic!("expected Text")
        };
        assert!(text.contains("profile/ commit_warning"), "{text}");
        let body = tokio::fs::read_to_string(&target).await.unwrap();
        assert!(body.contains("SEED"), "{body}");
    }
}
