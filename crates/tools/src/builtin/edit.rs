//! `Edit` — targeted string replacement inside an existing file.
//!
//! Edits that target an **identity file** pick up four extra guards before
//! the write:
//!
//! `<workspace>/personas/<agent_id>/{SOUL,IDENTITY,USER}.md` — that agent's
//! personality, self-image, and own notes about the human. The built-in is an
//! ordinary agent here (`personas/baybo/`). Only directly under the agent's
//! own directory: a persona's `skills/` tree is skill content rather than
//! identity, **another** agent's directory is not reachable at all, and the
//! shared `personas/USER.md` is nobody's identity file — an edit to it takes
//! the ordinary approval gate.
//!
//! The guards:
//!
//! - **Allowlist**: nothing else in either location may be touched.
//!   They are declarative stores, not freeform scratch space.
//! - **Size cap**: identity files are system-prompt-bound (~kB). A
//!   multi-MiB file is corruption or symlink shenanigans — fail before
//!   we slurp it.
//! - **The name survives**: an edit to `IDENTITY.md` may change the `Name:`
//!   line but not remove it — that line is what every surface calls the
//!   agent, and losing it fails nothing loudly, it just renders the agent as
//!   a raw id.
//! - **Audit commit**: after a successful write, the change is staged and
//!   committed into the `personas/` repo with a fixed `Baybo <baybo@local>`
//!   author, so the user can later see what the agent rewrote and revert
//!   with `git`.
//!   `--no-verify` is intentional: it is Baybo-managed audit history, not a
//!   hand-curated repo where pre-commit hooks would be authored. The
//!   directory is committed when it is materialised, so this commit is a
//!   diff against a real baseline rather than a file addition. A
//!   commit failure (detached HEAD, missing git, etc.) leaves the file
//!   write in place and surfaces a warning in the tool output.
//!
//! This is what keeps the system prompt's own instruction honest: it
//! names each identity file's absolute path and tells the model to
//! `Edit` it, so a self-edit must not be a per-turn approval prompt with
//! no audit trail — for *every* agent, not just the built-in.
//!
//! Edits under `<workspace>/work/` also skip the approval gate (matching
//! the `Write` tool's `work/` bypass), but without the identity-file
//! allowlist, size cap, or audit commit — that root is agent scratch, not
//! an identity store.
//!
//! A skill directory gets no bypass and no write at all: every skill belongs
//! to some agent, and `SkillInstall` — which runs the risk assessor — is its
//! only sanctioned writer.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use baybo_workspace::{IdentityKind, WorkspacePaths};
use serde::Deserialize;
use serde_json::{Value, json};

/// Named once: `name()`, rejection messages, and the `Tool:` trailer of
/// every audit commit have to agree.
pub(crate) const EDIT_TOOL_NAME: &str = "Edit";

/// Trailer on a persona-file edit, telling the model when the prompt it is
/// running on will reflect what it just wrote.
///
/// This used to say "on the next compaction or new session", which was true
/// until `baybo_context::ContextManager::reconcile_system_prompt` started
/// re-checking the identity files before every LLM call: the edit now reaches
/// the model as an appended delta on the very next request, and only folds back
/// into the leading system row at the next compaction. The model reads this line
/// immediately after editing its own soul, so a stale claim here is not a
/// comment — it is the model being told the opposite of what happens.
const PERSONA_EDIT_PICKUP_NOTE: &str =
    "\nnote: your system prompt picks this change up on the next request";

use super::managed_repo::{
    ChangeKind, ManagedRoots, ManagedTarget, append_audit_line, commit_change, reject_if_oversized,
};
use super::paths::require_absolute;
use crate::{ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

pub struct EditTool {
    roots: ManagedRoots,
}

impl EditTool {
    pub fn new(workspace_paths: WorkspacePaths) -> Self {
        Self {
            roots: ManagedRoots::new(&workspace_paths),
        }
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
        EDIT_TOOL_NAME
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
        // managed roots: identity files under `personas/` (audit trail
        // is the per-edit git commit, not a user prompt), and any path
        // under `work/` or `skills/` (agent scratch / managed skill
        // content). Anything else still goes through the gate.
        if self.roots.bypasses_approval(&path) {
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

        let audit_target = self
            .roots
            .audit_target(Path::new(&p.file_path), &ctx.agent_id)?;
        if audit_target.is_some() {
            // Edit slurps the whole file to replace inside it, so it is the
            // one verb that must refuse an oversized one.
            reject_if_oversized(&p.file_path)?;
        }

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

        if let Some(target) = &audit_target {
            reject_if_it_would_orphan_the_name(target, &contents, &updated)?;
        }

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

        if let Some(target) = audit_target {
            let outcome = commit_change(
                self.roots.personas_dir(),
                &target,
                ChangeKind::Update,
                EDIT_TOOL_NAME,
                ctx.session_id.as_str(),
            )
            .await;
            append_audit_line(&mut text, outcome);
            text.push_str(PERSONA_EDIT_PICKUP_NOTE);
        }

        Ok(ToolOutput::Text(text))
    }
}

/// Refuse an edit to `IDENTITY.md` that would leave it without a readable
/// `Name:` line.
///
/// That line is not prose: it is what every surface calls this agent — the
/// roster, the picker, the delete confirmation. Losing it does not fail
/// anything loudly, it just makes the agent render as its raw id, so an
/// incidental reformat while updating some other field could quietly cost
/// the agent its name. Only *removal* is refused: an edit is free to change
/// the name, and a file that had no name to begin with (the shipped
/// template, which invites the agent to choose one) is left alone.
fn reject_if_it_would_orphan_the_name(
    target: &ManagedTarget,
    before: &str,
    after: &str,
) -> crate::Result<()> {
    if !target
        .rel_path
        .ends_with(IdentityKind::Identity.file_name())
    {
        return Ok(());
    }
    if baybo_workspace::display_name(before).is_some()
        && baybo_workspace::display_name(after).is_none()
    {
        return Err(ToolError::InvalidParams(
            "this edit would remove the `Name:` line from IDENTITY.md, which is what every \
             surface calls you — keep a `Name: <something>` line (renaming yourself is fine)"
                .into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::AgentProfileId;

    /// The schema spells its parameter names inline, and `baybo-agent` reads
    /// the same names back off a recorded `ToolUse` through the shared consts.
    /// Two spellings of one name is exactly the drift worth a cheap guard.
    #[test]
    fn the_shared_arg_consts_match_the_schema() {
        let schema = EditTool::new(WorkspacePaths::new("/tmp")).parameters_schema();
        let required = schema["required"]
            .as_array()
            .expect("required is a list")
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>();
        for name in [
            crate::TOOL_FILE_PATH_ARG,
            crate::EDIT_OLD_STRING_ARG,
            crate::EDIT_NEW_STRING_ARG,
        ] {
            assert!(required.contains(&name), "{name} missing from `required`");
            assert!(
                schema["properties"].get(name).is_some(),
                "{name} missing from `properties`"
            );
        }
    }

    use baybo_model::{ChannelType, User};
    use baybo_workspace::WorkspacePaths;
    use std::time::Duration;
    use tokio::process::Command;

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

    fn tool() -> EditTool {
        EditTool::new(WorkspacePaths::new("/tmp"))
    }

    fn tool_with(paths: WorkspacePaths) -> EditTool {
        EditTool::new(paths)
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

    /// Every agent's soul lives under its own `personas/<id>/`, the built-in
    /// included. The self-edit the system prompt instructs must get the same
    /// treatment for each — no approval prompt, and an audit commit — or the
    /// instruction
    /// is only honest for the built-in agent.
    async fn make_persona_workspace(agent_id: &str) -> (tempfile::TempDir, WorkspacePaths) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = WorkspacePaths::new(tmp.path().to_path_buf());
        let personas = paths.personas_dir();
        tokio::fs::create_dir_all(paths.persona_skills_dir(agent_id))
            .await
            .unwrap();
        for kind in IdentityKind::all() {
            tokio::fs::write(
                paths.persona_identity_file(agent_id, kind),
                "## seed\nalpha\n",
            )
            .await
            .unwrap();
        }
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
        let target = paths.persona_identity_file(agent, IdentityKind::Soul);

        // Approval gate: an identity edit declares only a read, so the
        // agent is not prompted every time it updates its own persona.
        let declared = tool_with(paths.clone()).accessed_resources(&json!({ "file_path": target }));
        assert_eq!(
            declared.len(),
            1,
            "persona soul must bypass the write gate: {declared:?}"
        );
        assert!(matches!(declared[0], ResourceAccess::ReadFile { .. }));

        let ctx = ToolContext {
            agent_id: AgentProfileId::parse(agent).unwrap(),
            ..ctx_with_paths(paths.clone())
        };
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

    /// All three of an agent's identity files get the same treatment — and
    /// `USER.md` is in the set because those are *this* agent's notes about
    /// the human, not the shared profile.
    #[tokio::test]
    async fn every_persona_identity_file_is_editable_by_its_own_agent() {
        let agent_id = "01JAGENT";
        let (_tmp, paths) = make_persona_workspace(agent_id).await;
        let agent = AgentProfileId::parse(agent_id).unwrap();
        let ctx = ToolContext {
            agent_id: agent.clone(),
            ..ctx_with_paths(paths.clone())
        };

        for kind in [IdentityKind::Identity, IdentityKind::User] {
            let target = paths.persona_identity_file(agent_id, kind);
            let declared =
                tool_with(paths.clone()).accessed_resources(&json!({ "file_path": target }));
            assert_eq!(declared.len(), 1, "{kind:?} must bypass the write gate");

            let out = tool_with(paths.clone())
                .execute(
                    json!({
                        "file_path": target,
                        "old_string": "alpha",
                        "new_string": format!("{kind:?}-rewritten"),
                    }),
                    &ctx,
                )
                .await
                .unwrap();
            let ToolOutput::Text(text) = out else {
                panic!("expected text output")
            };
            assert!(text.contains("committed to personas/"), "{text}");
        }
    }

    /// The `Name:` line is what every surface calls the agent, so an edit
    /// may change it but not delete it — otherwise an incidental reformat
    /// costs the agent its name and it silently renders as a raw id.
    #[tokio::test]
    async fn an_edit_cannot_strip_the_name_from_its_identity_file() {
        let agent_id = "01JAGENT";
        let (_tmp, paths) = make_persona_workspace(agent_id).await;
        let target = paths.persona_identity_file(agent_id, IdentityKind::Identity);
        let named = "# Who Am I?\n\n* **Name:** Aster\n* **Vibe:** dry\n";
        tokio::fs::write(&target, named).await.unwrap();
        let ctx = ToolContext {
            agent_id: AgentProfileId::parse(agent_id).unwrap(),
            ..ctx_with_paths(paths.clone())
        };

        let err = tool_with(paths.clone())
            .execute(
                json!({
                    "file_path": target,
                    "old_string": "* **Name:** Aster\n",
                    "new_string": "",
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParams(ref m) if m.contains("Name:")),
            "got: {err:?}"
        );
        assert_eq!(
            tokio::fs::read_to_string(&target).await.unwrap(),
            named,
            "a refused edit writes nothing"
        );

        // Renaming is fine — only losing the line is refused.
        tool_with(paths.clone())
            .execute(
                json!({
                    "file_path": target,
                    "old_string": "Aster",
                    "new_string": "Vega",
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(
            baybo_workspace::display_name(&tokio::fs::read_to_string(&target).await.unwrap())
                .as_deref(),
            Some("Vega")
        );

        // And a file that never had a name is not held hostage by the guard.
        let unnamed = paths.persona_identity_file(agent_id, IdentityKind::Soul);
        tokio::fs::write(&unnamed, "alpha\n").await.unwrap();
        tool_with(paths.clone())
            .execute(
                json!({ "file_path": unnamed, "old_string": "alpha", "new_string": "bravo" }),
                &ctx,
            )
            .await
            .unwrap();
    }

    /// One agent must not reach into another's persona. The approval
    /// declaration is shape-based (it has no call context), so the guard that
    /// matters is the refusal here — nothing is written.
    #[tokio::test]
    async fn one_agent_cannot_edit_anothers_identity_files() {
        let victim_id = "01JVICTIM";
        let (_tmp, paths) = make_persona_workspace(victim_id).await;
        let target = paths.persona_identity_file(victim_id, IdentityKind::Soul);
        let ctx = ToolContext {
            agent_id: AgentProfileId::parse("01JATTACKER").unwrap(),
            ..ctx_with_paths(paths.clone())
        };

        let err = tool_with(paths.clone())
            .execute(
                json!({
                    "file_path": target,
                    "old_string": "alpha",
                    "new_string": "hijacked",
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParams(ref m) if m.contains("this agent's own")),
            "got: {err:?}"
        );
        assert_eq!(
            tokio::fs::read_to_string(&target).await.unwrap(),
            "## seed\nalpha\n",
            "the other agent's file must be untouched"
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
            matches!(err, ToolError::InvalidParams(ref m) if m.contains("SOUL,IDENTITY")),
            "got: {err:?}"
        );
        assert_eq!(
            tokio::fs::read_to_string(&stray).await.unwrap(),
            "alpha\n",
            "file must not be modified on guard reject"
        );
    }
}
