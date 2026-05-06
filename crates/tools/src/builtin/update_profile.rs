//! `UpdateProfile` — write to one of the workspace identity files
//! (`profile/{SOUL,USER,IDENTITY}.md`) and commit the change to
//! `profile/`'s standalone git repo for audit history.
//!
//! Why a dedicated tool instead of `Write`/`Edit`:
//!
//! - **Bounded scope**: targets keyed by [`IdentityKind`], not arbitrary
//!   paths — `profile/` stays a tidy declarative dir of three known
//!   files.
//! - **Versioned audit trail**: every successful change lands as its own
//!   commit (single file staged, fixed `Aura <aura@local>` author) so a
//!   user can later see what the agent changed and revert.
//! - **Out-of-band notification**: the tool emits a `NoticeLevel::Info`
//!   so the user sees the change even when the LLM does not surface it
//!   in its next reply.
//!
//! Identity files load into the system prompt at `AgentLoop`
//! construction (`aura-agent::Soul`), so a write here does **not**
//! affect the *current* session's behaviour — it takes effect on the
//! next session. The TODO doc at `docs/todo/profile-hot-reload.md`
//! tracks the future work to lift that limitation.
//!
//! `--no-verify` is intentional: `profile/` is Aura-managed audit
//! history, not a hand-curated user repo where pre-commit hooks would
//! be authored.

use std::path::Path;

use async_trait::async_trait;
use aura_workspace::IdentityKind;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::{NoticeLevel, ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

const GIT_AUTHOR_NAME: &str = "Aura";
const GIT_AUTHOR_EMAIL: &str = "aura@local";

pub struct UpdateProfileTool;

#[derive(Debug, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum ModeParams {
    Replace {
        old_string: String,
        new_string: String,
        #[serde(default)]
        replace_all: bool,
    },
    Append {
        content: String,
    },
}

#[derive(Debug, Deserialize)]
struct Params {
    target: IdentityTarget,
    #[serde(flatten)]
    mode: ModeParams,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum IdentityTarget {
    Soul,
    User,
    Identity,
}

impl From<IdentityTarget> for IdentityKind {
    fn from(t: IdentityTarget) -> Self {
        match t {
            IdentityTarget::Soul => IdentityKind::Soul,
            IdentityTarget::User => IdentityKind::User,
            IdentityTarget::Identity => IdentityKind::Identity,
        }
    }
}

#[async_trait]
impl Tool for UpdateProfileTool {
    fn name(&self) -> &str {
        "UpdateProfile"
    }

    fn description(&self) -> &str {
        "Update one of the agent's identity files (profile/SOUL.md, \
         profile/USER.md, profile/IDENTITY.md) and commit the change to \
         the profile/ git repo. Use this — not Write/Edit — for changes \
         to identity content.\n\n\
         MODES:\n\
         - mode=replace: replaces `old_string` with `new_string`. \
           `old_string` must occur exactly once unless `replace_all` is \
           true. If the match fails, the tool returns the absolute path \
           of the file so you can re-read it and retry.\n\
         - mode=append: appends `content` to the end of the file, \
           inserting a newline boundary if the file did not end with one.\n\n\
         TARGETS: `target` ∈ {soul, user, identity}; arbitrary file \
         paths are not supported.\n\n\
         EFFECT: changes take effect on the *next* session — the current \
         session's system prompt was baked at AgentLoop construction and \
         is not re-read mid-session. The user is notified out-of-band on \
         success."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "target": { "type": "string", "enum": ["soul", "user", "identity"] },
                "mode":   { "type": "string", "enum": ["replace", "append"] },
                "old_string":  { "type": "string" },
                "new_string":  { "type": "string" },
                "replace_all": { "type": "boolean", "default": false },
                "content":     { "type": "string", "minLength": 1 }
            },
            "required": ["target", "mode"],
            "oneOf": [
                {
                    "properties": { "mode": { "const": "replace" } },
                    "required": ["old_string", "new_string"]
                },
                {
                    "properties": { "mode": { "const": "append" } },
                    "required": ["content"]
                }
            ]
        })
    }

    fn accessed_resources(&self, _params: &Value) -> Vec<ResourceAccess> {
        // No approval gate; visibility via post-success Notice + profile/ git history.
        Vec::new()
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        let kind: IdentityKind = p.target.into();
        let path = ctx.workspace_paths.identity_file(kind);
        let profile_dir = ctx.workspace_paths.profile_dir();
        let file_name = kind.file_name();

        // Identity files are system-prompt-bound (~kB). A multi-MiB profile.md
        // is corruption or symlink shenanigans — fail before we slurp it.
        const MAX_PROFILE_BYTES: u64 = 1 << 20;
        match tokio::fs::metadata(&path).await {
            Ok(m) if m.len() > MAX_PROFILE_BYTES => {
                return Err(ToolError::Execution(format!(
                    "profile file {} is {} bytes (>{} MiB cap); refusing to load",
                    path.display(),
                    m.len(),
                    MAX_PROFILE_BYTES >> 20
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ToolError::Execution(format!(
                    "profile file missing: {} — run `aura setup` to seed defaults",
                    path.display()
                )));
            }
            Err(e) => {
                return Err(ToolError::Execution(format!(
                    "stat {}: {e}",
                    path.display()
                )));
            }
        }

        let original = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| ToolError::Execution(format!("read {}: {e}", path.display())))?;

        let (new_content, mode_label) = match &p.mode {
            ModeParams::Replace {
                old_string,
                new_string,
                replace_all,
            } => {
                if old_string == new_string {
                    return Err(ToolError::InvalidParams(
                        "old_string and new_string are identical".into(),
                    ));
                }
                let count = original.matches(old_string.as_str()).count();
                if count == 0 {
                    return Err(ToolError::Execution(format!(
                        "old_string not found in {}; re-read the file and retry",
                        path.display()
                    )));
                }
                if !*replace_all && count > 1 {
                    return Err(ToolError::Execution(format!(
                        "old_string occurs {count} times in {}; supply more surrounding \
                         context to make it unique, or set replace_all=true. Re-read the \
                         file and retry.",
                        path.display()
                    )));
                }
                let updated = if *replace_all {
                    original.replace(old_string.as_str(), new_string)
                } else {
                    original.replacen(old_string.as_str(), new_string, 1)
                };
                (updated, "replace")
            }
            ModeParams::Append { content } => {
                if content.is_empty() {
                    return Err(ToolError::InvalidParams(
                        "append content must not be empty".into(),
                    ));
                }
                let mut updated = original.clone();
                if !updated.is_empty() && !updated.ends_with('\n') {
                    updated.push('\n');
                }
                updated.push_str(content);
                if !updated.ends_with('\n') {
                    updated.push('\n');
                }
                (updated, "append")
            }
        };

        tokio::fs::write(&path, &new_content)
            .await
            .map_err(|e| ToolError::Execution(format!("write {}: {e}", path.display())))?;

        let commit_outcome =
            commit_change(&profile_dir, file_name, mode_label, ctx.session_id.as_str()).await;

        let (committed, commit_sha, commit_warning) = match commit_outcome {
            Ok(sha) => (true, Some(sha), None),
            Err(reason) => (false, None, Some(reason)),
        };

        emit_notice(ctx, file_name, committed);

        let mut text =
            format!("updated profile/{file_name}\nmode: {mode_label}\ncommitted: {committed}\n");
        if let Some(sha) = &commit_sha {
            text.push_str(&format!("commit: {sha}\n"));
        }
        if let Some(reason) = &commit_warning {
            text.push_str(&format!("commit_warning: {reason}\n"));
        }
        text.push_str(
            "note: change takes effect on the next session; current Soul prompt unaffected\n",
        );

        Ok(ToolOutput::Text(text))
    }
}

fn emit_notice(ctx: &ToolContext, file_name: &str, committed: bool) {
    let Some(notifier) = ctx.notifier.as_ref() else {
        return;
    };
    let (level, summary) = if committed {
        (NoticeLevel::Info, format!("Updated {file_name}"))
    } else {
        (
            NoticeLevel::Warn,
            format!("Updated {file_name} (uncommitted)"),
        )
    };
    notifier.emit(level, &summary, "Change takes effect on next session.");
}

async fn commit_change(
    profile_dir: &Path,
    file_name: &str,
    mode_label: &str,
    session_id: &str,
) -> Result<String, String> {
    if !is_on_branch(profile_dir).await? {
        return Err("HEAD is detached; check out a branch in profile/ first".into());
    }

    // Need an explicit `git add` first: `git commit --only -- <path>` does
    // not stage untracked entries, and on a fresh workspace `aura setup`
    // seeds profile/{SOUL,USER,IDENTITY}.md without an initial commit, so
    // the first UpdateProfile call sees an untracked target.
    run_git(profile_dir, &[], &["add", "--", file_name]).await?;

    let commit_msg = format!(
        "profile: update {file_name} ({mode_label})\n\n\
         Tool: UpdateProfile\n\
         Mode: {mode_label}\n\
         Session: {session_id}\n",
    );
    run_git(
        profile_dir,
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

    let sha = run_git(profile_dir, &[], &["rev-parse", "--short", "HEAD"]).await?;
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
    use crate::{NoticeLevel as ToolsNoticeLevel, SessionNotifier};
    use aura_model::{ChannelType, User};
    use aura_workspace::WorkspacePaths;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    #[derive(Default)]
    struct CapturedNotifier {
        events: Mutex<Vec<(ToolsNoticeLevel, String, String)>>,
    }

    impl SessionNotifier for CapturedNotifier {
        fn emit(&self, level: ToolsNoticeLevel, summary: &str, detail: &str) {
            self.events
                .lock()
                .push((level, summary.to_string(), detail.to_string()));
        }
    }

    async fn run_git_quiet(dir: &Path, args: &[&str]) {
        let status = tokio::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .await
            .expect("git");
        assert!(status.success(), "git {args:?} failed");
    }

    async fn make_workspace() -> (tempfile::TempDir, WorkspacePaths) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let paths = WorkspacePaths::new(root.clone());
        let profile = paths.profile_dir();
        tokio::fs::create_dir_all(&profile).await.unwrap();
        run_git_quiet(&profile, &["init", "--quiet", "-b", "main"]).await;
        for kind in IdentityKind::all() {
            tokio::fs::write(paths.identity_file(kind), "## seed\n")
                .await
                .unwrap();
        }
        // Initial commit so HEAD points to a real branch tip.
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

    /// Workspace shape that matches `aura setup`'s actual output: profile/
    /// is `git init`-ed with the three identity files seeded but **no
    /// initial commit**, so the files are untracked. Used to exercise the
    /// fresh-workspace path where UpdateProfile must stage before commit.
    async fn make_uninitialized_workspace() -> (tempfile::TempDir, WorkspacePaths) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let paths = WorkspacePaths::new(root.clone());
        let profile = paths.profile_dir();
        tokio::fs::create_dir_all(&profile).await.unwrap();
        run_git_quiet(&profile, &["init", "--quiet", "-b", "main"]).await;
        for kind in IdentityKind::all() {
            tokio::fs::write(paths.identity_file(kind), "## seed\n")
                .await
                .unwrap();
        }
        (tmp, paths)
    }

    fn make_ctx(paths: WorkspacePaths, notifier: Option<Arc<dyn SessionNotifier>>) -> ToolContext {
        ToolContext {
            session_id: "sess-test".into(),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            timeout: Duration::from_secs(10),
            cancellation_token: CancellationToken::new(),
            workspace_root: paths.work_dir(),
            workspace_paths: paths,
            sandbox: None,
            approval: None,
            notifier,
        }
    }

    #[tokio::test]
    async fn replace_happy_path_commits_with_aura_author() {
        let (_tmp, paths) = make_workspace().await;
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

        let notifier = Arc::new(CapturedNotifier::default());
        let ctx = make_ctx(paths.clone(), Some(notifier.clone()));
        let out = UpdateProfileTool
            .execute(
                json!({
                    "target": "soul",
                    "mode": "replace",
                    "old_string": "bravo",
                    "new_string": "BRAVO",
                }),
                &ctx,
            )
            .await
            .expect("execute");

        let body = tokio::fs::read_to_string(&target).await.unwrap();
        assert_eq!(body, "alpha BRAVO charlie\n");

        let ToolOutput::Text(text) = out else {
            panic!("expected Text output")
        };
        assert!(text.contains("committed: true"), "{text}");
        assert!(text.contains("mode: replace"), "{text}");
        assert!(text.contains("commit: "), "{text}");

        {
            let events = notifier.events.lock();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].0, ToolsNoticeLevel::Info);
            assert_eq!(events[0].1, "Updated SOUL.md");
            assert_eq!(events[0].2, "Change takes effect on next session.");
        }

        let log = tokio::process::Command::new("git")
            .arg("-C")
            .arg(paths.profile_dir())
            .args(["log", "-1", "--pretty=%an <%ae>%n%B"])
            .output()
            .await
            .unwrap();
        let log = String::from_utf8_lossy(&log.stdout);
        assert!(log.contains("Aura <aura@local>"), "{log}");
        assert!(log.contains("Tool: UpdateProfile"), "{log}");
        assert!(log.contains("Mode: replace"), "{log}");
        assert!(log.contains("Session: sess-test"), "{log}");
    }

    #[tokio::test]
    async fn replace_old_string_not_found_returns_path() {
        let (_tmp, paths) = make_workspace().await;
        let ctx = make_ctx(paths.clone(), None);
        let target = paths.identity_file(IdentityKind::Soul);
        let err = UpdateProfileTool
            .execute(
                json!({
                    "target": "soul",
                    "mode": "replace",
                    "old_string": "nope",
                    "new_string": "x",
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(target.to_string_lossy().as_ref()), "{msg}");
        assert!(msg.contains("re-read"), "{msg}");
    }

    #[tokio::test]
    async fn replace_non_unique_returns_path() {
        let (_tmp, paths) = make_workspace().await;
        let target = paths.identity_file(IdentityKind::User);
        tokio::fs::write(&target, "x x x\n").await.unwrap();
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
                "duplicate",
            ],
        )
        .await;
        let ctx = make_ctx(paths.clone(), None);
        let err = UpdateProfileTool
            .execute(
                json!({
                    "target": "user",
                    "mode": "replace",
                    "old_string": "x",
                    "new_string": "y",
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(target.to_string_lossy().as_ref()), "{msg}");
        assert!(msg.contains("3 times"), "{msg}");
    }

    #[tokio::test]
    async fn replace_identical_strings_is_invalid_params() {
        let (_tmp, paths) = make_workspace().await;
        let ctx = make_ctx(paths.clone(), None);
        let err = UpdateProfileTool
            .execute(
                json!({
                    "target": "soul",
                    "mode": "replace",
                    "old_string": "seed",
                    "new_string": "seed",
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn append_appends_with_trailing_newline() {
        let (_tmp, paths) = make_workspace().await;
        let target = paths.identity_file(IdentityKind::Identity);
        // File ends with "\n" already from seed; verify no double newline.
        let ctx = make_ctx(paths.clone(), None);
        UpdateProfileTool
            .execute(
                json!({
                    "target": "identity",
                    "mode": "append",
                    "content": "new line",
                }),
                &ctx,
            )
            .await
            .expect("execute");
        let body = tokio::fs::read_to_string(&target).await.unwrap();
        assert_eq!(body, "## seed\nnew line\n");
    }

    #[tokio::test]
    async fn append_empty_content_is_invalid_params() {
        let (_tmp, paths) = make_workspace().await;
        let ctx = make_ctx(paths.clone(), None);
        let err = UpdateProfileTool
            .execute(
                json!({
                    "target": "soul",
                    "mode": "append",
                    "content": "",
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn missing_file_errors_with_setup_hint() {
        let (_tmp, paths) = make_workspace().await;
        let target = paths.identity_file(IdentityKind::Soul);
        tokio::fs::remove_file(&target).await.unwrap();
        let ctx = make_ctx(paths.clone(), None);
        let err = UpdateProfileTool
            .execute(
                json!({
                    "target": "soul",
                    "mode": "append",
                    "content": "x",
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("aura setup"), "{msg}");
    }

    #[tokio::test]
    async fn fresh_workspace_first_call_creates_initial_commit() {
        // Mirrors `aura setup`: profile/ is git init'd, identity files
        // exist on disk, but nothing has been committed yet — so the
        // target file is untracked.
        let (_tmp, paths) = make_uninitialized_workspace().await;
        let ctx = make_ctx(paths.clone(), None);
        let out = UpdateProfileTool
            .execute(
                json!({
                    "target": "soul",
                    "mode": "append",
                    "content": "first edit",
                }),
                &ctx,
            )
            .await
            .expect("first call on fresh workspace");
        let ToolOutput::Text(text) = out else {
            panic!("expected Text")
        };
        assert!(
            text.contains("committed: true"),
            "first call must produce a real commit, not committed: false; got:\n{text}"
        );
        // Confirm via git that HEAD now resolves and the file is tracked.
        let log = tokio::process::Command::new("git")
            .arg("-C")
            .arg(paths.profile_dir())
            .args(["log", "--oneline"])
            .output()
            .await
            .unwrap();
        let log = String::from_utf8_lossy(&log.stdout);
        assert!(!log.is_empty(), "git log should show the new commit");
    }

    #[tokio::test]
    async fn detached_head_skips_commit_and_warns() {
        let (_tmp, paths) = make_workspace().await;
        // Detach HEAD by checking out the current commit by sha.
        let head = tokio::process::Command::new("git")
            .arg("-C")
            .arg(paths.profile_dir())
            .args(["rev-parse", "HEAD"])
            .output()
            .await
            .unwrap();
        let sha = String::from_utf8_lossy(&head.stdout).trim().to_string();
        run_git_quiet(&paths.profile_dir(), &["checkout", "--quiet", &sha]).await;

        let notifier = Arc::new(CapturedNotifier::default());
        let ctx = make_ctx(paths.clone(), Some(notifier.clone()));
        let out = UpdateProfileTool
            .execute(
                json!({
                    "target": "soul",
                    "mode": "append",
                    "content": "detached add",
                }),
                &ctx,
            )
            .await
            .expect("execute should still succeed with warning");
        let ToolOutput::Text(text) = out else {
            panic!("expected Text")
        };
        assert!(text.contains("committed: false"), "{text}");
        assert!(text.contains("commit_warning"), "{text}");
        // File update did happen.
        let body = tokio::fs::read_to_string(paths.identity_file(IdentityKind::Soul))
            .await
            .unwrap();
        assert!(body.ends_with("detached add\n"), "{body}");

        let events = notifier.events.lock();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, ToolsNoticeLevel::Warn);
        assert_eq!(events[0].1, "Updated SOUL.md (uncommitted)");
    }
}
