//! `MemoryDelete` — remove one file from a memory tree.
//!
//! Memory is only as useful as it is accurate, so forgetting has to be as
//! ordinary an act as remembering. `Write` and `Edit` already treat a
//! memory file as audited-not-approved; this is the third verb.
//!
//! It deletes only files in the calling agent's own tree — the same
//! ownership rule its identity files carry.
//!
//! It exists as its own tool rather than leaning on `Bash rm` because that
//! gate is permission-mode-dependent: under the default `auto` mode an LLM
//! risk judge can wave a scoped `rm` through unprompted, under `manual` it
//! routes to a channel approval that an unattended dream pass will never
//! get answered, and under `free` there is no destructive check at all. A
//! dedicated tool makes deletion **deterministic, root-scoped, and
//! audited** whatever the bash mode is: it refuses any path outside a
//! memory tree, and every removal lands as a git commit that `git revert`
//! can undo.
//!
//! (The repo's never-delete rule is about session rows and transcripts. A
//! memory file is agent-authored content with a recoverable history —
//! deleting one is the intended maintenance path, not data loss.)

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use baybo_workspace::WorkspacePaths;
use baybo_workspace::paths::MEMORY_INDEX_FILE;
use serde::Deserialize;
use serde_json::{Value, json};

use super::managed_repo::{ChangeKind, ManagedRoots, append_audit_line, commit_change};
use super::paths::require_absolute;
use crate::{ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

/// Named once: `name()`, rejection messages, and the `Tool:` trailer of
/// every audit commit have to agree.
const MEMORY_DELETE_TOOL_NAME: &str = "MemoryDelete";

const DESCRIPTION: &str = r#"Delete one file from your memory directory, when what it records has turned out to be wrong, obsolete, or superseded by another memory.

Only paths inside a memory tree are accepted — anything else is refused outright, so this can never be used as a general-purpose delete. Remove the file's line from MEMORY.md in the same breath, or the index will point at nothing.

The deletion is committed to the memory directory's git history, so it can be recovered later if it turns out to have been a mistake."#;

pub struct MemoryDeleteTool {
    roots: ManagedRoots,
}

impl MemoryDeleteTool {
    pub fn new(workspace_paths: WorkspacePaths) -> Self {
        Self {
            roots: ManagedRoots::new(&workspace_paths),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Params {
    file_path: PathBuf,
}

impl MemoryDeleteTool {
    /// The index path, when it still mentions the just-deleted file.
    ///
    /// The tool does **not** edit `MEMORY.md` itself: it is markdown the
    /// model authors in whatever shape it likes, and a tool rewriting lines
    /// by pattern would eventually mangle one. But a dangling entry is the
    /// one thing the index cannot tolerate — it rides every prompt and
    /// would send the model to `Read` a path that is gone — so a cheap scan
    /// is enough to say so out loud rather than let it rot silently.
    ///
    /// Per line rather than over the whole file, because a whole-file
    /// `contains` reports deleting `a.md` as still-named by a line about
    /// `data.md`. Per line the name still has to be *bounded* by
    /// non-filename characters, which a markdown link (`](a.md)`) satisfies
    /// and a longer filename does not.
    async fn index_still_names(&self, deleted: &Path) -> Option<String> {
        let name = deleted.file_name()?.to_str()?;
        let index = deleted.parent()?.join(MEMORY_INDEX_FILE);
        // Deleting the index itself leaves nothing to be inconsistent with.
        if index == deleted {
            return None;
        }
        let body = tokio::fs::read_to_string(&index).await.ok()?;
        body.lines()
            .any(|line| line_names_file(line, name))
            .then(|| index.display().to_string())
    }
}

/// Whether `line` names exactly `file`, rather than merely containing its
/// characters. A filename is bounded here by anything that cannot be part of
/// one — `](data.md)` must not read as naming `a.md`.
fn line_names_file(line: &str, file: &str) -> bool {
    let is_name_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.');
    line.match_indices(file).any(|(at, _)| {
        let before_ok = line[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !is_name_char(c));
        let after_ok = line[at + file.len()..]
            .chars()
            .next()
            .is_none_or(|c| !is_name_char(c));
        before_ok && after_ok
    })
}

#[async_trait]
impl Tool for MemoryDeleteTool {
    fn name(&self) -> &str {
        MEMORY_DELETE_TOOL_NAME
    }

    fn description(&self) -> String {
        DESCRIPTION.to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path of the memory file to delete"
                }
            },
            "required": ["file_path"],
            "additionalProperties": false
        })
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        params
            .get("file_path")
            .and_then(Value::as_str)
            .map(|s| crate::progress::preview_path(Path::new(s)))
    }

    fn accessed_resources(&self, _params: &Value) -> Vec<ResourceAccess> {
        // Nothing here for a gate to decide. A real memory file needs no
        // prompt — the audit commit is the accountability — and anything
        // else is refused by `execute` before it touches the disk, so a
        // prompt could only ask the user to approve a deletion that cannot
        // happen either way. Asking anyway would teach them that approving
        // this tool is harmless, which is the opposite of what a gate is
        // for; the refusal is already in the tool result and the trace.
        Vec::new()
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        require_absolute(&p.file_path, MEMORY_DELETE_TOOL_NAME, "file_path")?;

        let target = self
            .roots
            .memory_target(Path::new(&p.file_path), &ctx.agent_id)?
            .ok_or_else(|| {
                ToolError::InvalidParams(format!(
                    "{MEMORY_DELETE_TOOL_NAME} only deletes files inside a memory directory; {} is not one",
                    p.file_path.display()
                ))
            })?;

        tokio::fs::remove_file(&p.file_path)
            .await
            .map_err(|e| ToolError::Execution(format!("delete {}: {e}", p.file_path.display())))?;

        let mut text = format!("deleted {}", p.file_path.display());
        if let Some(orphan) = self.index_still_names(&p.file_path).await {
            text.push_str(&format!(
                "\nnote: {orphan} still lists this file — remove its line, or the index \
                 will point at nothing"
            ));
        }
        let outcome = commit_change(
            self.roots.personas_dir(),
            &target,
            ChangeKind::Remove,
            MEMORY_DELETE_TOOL_NAME,
            ctx.session_id.as_str(),
        )
        .await;
        append_audit_line(&mut text, outcome);

        Ok(ToolOutput::Text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::{AgentProfileId, ChannelType, User};
    use std::time::Duration;
    use tokio::process::Command;

    const AGENT: &str = "baybo";

    /// The warning is only worth emitting when it is true. A whole-file
    /// `contains` reports deleting `a.md` as still-indexed by a line about
    /// `data.md`, and a note that cries wolf is one the model learns to skip.
    #[test]
    fn the_index_check_matches_a_whole_filename_not_a_substring() {
        assert!(line_names_file("- [Title](a.md) — hook", "a.md"));
        assert!(line_names_file("a.md", "a.md"));
        assert!(!line_names_file("- [Data](data.md) — hook", "a.md"));
        assert!(!line_names_file("- [Other](a.markdown) — hook", "a.md"));
        assert!(!line_names_file("- [Nested](sub/xa.md) — hook", "a.md"));
    }

    fn ctx() -> ToolContext {
        ToolContext {
            session_id: "sess-test".into(),
            agent_id: AgentProfileId::parse(AGENT).expect("valid id"),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::owner(),
            },
            timeout: Duration::from_secs(5),
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
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }

    /// A workspace whose `memory/` is a real git repo holding one committed
    /// memory file, returned with the path to that file.
    async fn workspace_with_memory(dir: &Path) -> (WorkspacePaths, PathBuf) {
        let paths = WorkspacePaths::new(dir.to_path_buf());
        let personas = paths.personas_dir();
        let memory = paths.persona_memory_dir(AGENT);
        tokio::fs::create_dir_all(&memory).await.expect("mkdir");
        run_git_quiet(&personas, &["init", "--quiet", "-b", "main"]).await;

        let fact = memory.join("cat-name.md");
        tokio::fs::write(&fact, "---\nname: cat-name\n---\n\nMochi.\n")
            .await
            .expect("seed fact");
        tokio::fs::write(
            paths.persona_memory_index_file(AGENT),
            "- [Cat](cat-name.md) — Mochi\n",
        )
        .await
        .expect("seed index");
        run_git_quiet(&personas, &["add", "-A"]).await;
        run_git_quiet(
            &personas,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@local",
                "commit",
                "--quiet",
                "-m",
                "seed",
            ],
        )
        .await;
        (paths, fact)
    }

    #[tokio::test]
    async fn deletes_a_memory_file_and_commits_the_removal() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (paths, fact) = workspace_with_memory(tmp.path()).await;
        let tool = MemoryDeleteTool::new(paths.clone());

        let out = tool
            .execute(json!({ "file_path": fact.to_string_lossy() }), &ctx())
            .await
            .expect("delete");

        assert!(!fact.exists(), "file must be gone");
        let ToolOutput::Text(text) = out else {
            panic!("expected text output")
        };
        assert!(text.contains("committed to personas/"), "got: {text}");

        let log = Command::new("git")
            .arg("-C")
            .arg(paths.personas_dir())
            .args(["log", "-1", "--pretty=%an <%ae>%n%B"])
            .output()
            .await
            .expect("git log");
        let log = String::from_utf8_lossy(&log.stdout);
        assert!(log.contains("Baybo <baybo@local>"), "got: {log}");
        assert!(log.contains("Tool: MemoryDelete"), "got: {log}");
        assert!(log.contains("Session: sess-test"), "got: {log}");
        assert!(
            log.contains("remove baybo/memory/cat-name.md"),
            "got: {log}"
        );
    }

    #[tokio::test]
    async fn refuses_a_path_outside_a_memory_tree_without_touching_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (paths, _) = workspace_with_memory(tmp.path()).await;
        let soul = paths.persona_identity_file(AGENT, baybo_workspace::IdentityKind::Soul);
        tokio::fs::write(&soul, "the soul")
            .await
            .expect("seed soul");

        let tool = MemoryDeleteTool::new(paths.clone());
        let err = tool
            .execute(json!({ "file_path": soul.to_string_lossy() }), &ctx())
            .await
            .expect_err("must refuse");

        assert!(matches!(err, ToolError::InvalidParams(_)), "got: {err:?}");
        assert!(soul.exists(), "the refused target must survive");
    }

    #[tokio::test]
    async fn refuses_a_walk_out_of_the_memory_tree() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (paths, _) = workspace_with_memory(tmp.path()).await;
        let soul = paths.persona_identity_file(AGENT, baybo_workspace::IdentityKind::Soul);
        tokio::fs::write(&soul, "the soul")
            .await
            .expect("seed soul");

        // Lexically under memory/, actually the shared soul. The memory
        // tier has no filename allowlist, so this walk is the whole attack.
        let escape = paths.persona_memory_dir(AGENT).join("..").join("SOUL.md");
        let tool = MemoryDeleteTool::new(paths.clone());
        let err = tool
            .execute(json!({ "file_path": escape.to_string_lossy() }), &ctx())
            .await
            .expect_err("must refuse");

        assert!(matches!(err, ToolError::InvalidParams(_)), "got: {err:?}");
        assert!(soul.exists(), "the soul must survive the walk");
    }

    #[tokio::test]
    async fn a_delete_that_orphans_an_index_line_says_so() {
        // The tool will not rewrite the model's markdown, but a dangling
        // entry rides every prompt and sends it to `Read` a path that is
        // gone — so it must not rot silently.
        let tmp = tempfile::tempdir().expect("tempdir");
        let (paths, fact) = workspace_with_memory(tmp.path()).await;
        let tool = MemoryDeleteTool::new(paths.clone());

        let out = tool
            .execute(json!({ "file_path": fact.to_string_lossy() }), &ctx())
            .await
            .expect("delete");
        let ToolOutput::Text(text) = out else {
            panic!("expected text output")
        };
        assert!(text.contains("still lists this file"), "got: {text}");

        // …and it stays quiet once the index no longer names it.
        let second = paths.persona_memory_dir(AGENT).join("other.md");
        tokio::fs::write(&second, "unrelated").await.expect("seed");
        let out = tool
            .execute(json!({ "file_path": second.to_string_lossy() }), &ctx())
            .await
            .expect("delete");
        let ToolOutput::Text(text) = out else {
            panic!("expected text output")
        };
        assert!(!text.contains("still lists this file"), "got: {text}");
    }

    #[tokio::test]
    async fn an_oversized_memory_can_still_be_deleted() {
        // The size cap exists so `Edit` does not slurp a corrupted blob to
        // replace one byte. Applied to deletion it would be a trap: a file
        // that crossed the cap could not be read, written OR removed, and
        // if it were `MEMORY.md` it would ride every prompt at that size
        // until someone opened a shell.
        let tmp = tempfile::tempdir().expect("tempdir");
        let (paths, _) = workspace_with_memory(tmp.path()).await;
        let huge = paths.persona_memory_dir(AGENT).join("runaway.md");
        tokio::fs::write(&huge, "x".repeat((1 << 20) + 1))
            .await
            .expect("seed an oversized memory");

        let tool = MemoryDeleteTool::new(paths.clone());
        tool.execute(json!({ "file_path": huge.to_string_lossy() }), &ctx())
            .await
            .expect("an oversized memory must still be removable");

        assert!(!huge.exists());
    }

    #[tokio::test]
    async fn another_agents_memory_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (paths, _) = workspace_with_memory(tmp.path()).await;
        let theirs = paths.persona_memory_dir("01JSCOUT");
        tokio::fs::create_dir_all(&theirs).await.expect("mkdir");
        let fact = theirs.join("stale.md");
        tokio::fs::write(&fact, "old news").await.expect("seed");

        let tool = MemoryDeleteTool::new(paths.clone());
        let err = tool
            .execute(json!({ "file_path": fact.to_string_lossy() }), &ctx())
            .await
            .expect_err("each agent keeps its own memory");

        assert!(matches!(err, ToolError::InvalidParams(_)), "got: {err:?}");
        assert!(fact.exists(), "the refused target must survive");
    }

    #[tokio::test]
    async fn a_failed_commit_still_deletes_the_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (paths, fact) = workspace_with_memory(tmp.path()).await;
        let head = Command::new("git")
            .arg("-C")
            .arg(paths.personas_dir())
            .args(["rev-parse", "HEAD"])
            .output()
            .await
            .expect("rev-parse");
        let sha = String::from_utf8_lossy(&head.stdout).trim().to_string();
        run_git_quiet(&paths.personas_dir(), &["checkout", "--quiet", &sha]).await;

        let tool = MemoryDeleteTool::new(paths.clone());
        let out = tool
            .execute(json!({ "file_path": fact.to_string_lossy() }), &ctx())
            .await
            .expect("delete");

        assert!(
            !fact.exists(),
            "the removal is not undone by a failed audit"
        );
        let ToolOutput::Text(text) = out else {
            panic!("expected text output")
        };
        assert!(text.contains("commit_warning"), "got: {text}");
    }

    #[tokio::test]
    async fn nothing_about_this_tool_is_worth_prompting_about() {
        // A memory file is audited rather than approved; anything else is
        // refused outright. Prompting for the latter would ask the user
        // about a deletion that cannot happen.
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());
        let tool = MemoryDeleteTool::new(paths.clone());

        for path in [
            paths
                .persona_memory_dir(AGENT)
                .join("fact.md")
                .to_string_lossy()
                .into_owned(),
            "/tmp/not-a-memory.md".to_string(),
            paths
                .persona_identity_file(AGENT, baybo_workspace::IdentityKind::Soul)
                .to_string_lossy()
                .into_owned(),
        ] {
            assert!(
                tool.accessed_resources(&json!({ "file_path": path }))
                    .is_empty(),
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn rejects_a_relative_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());
        let tool = MemoryDeleteTool::new(paths);

        let err = tool
            .execute(json!({ "file_path": "memory/fact.md" }), &ctx())
            .await
            .expect_err("must refuse");
        assert!(matches!(err, ToolError::InvalidParams(_)), "got: {err:?}");
    }
}
