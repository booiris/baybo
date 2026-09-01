//! `Write` — create or overwrite a file with the given contents.
//!
//! Writes into this agent's own **memory tree**
//! (`<workspace>/personas/<agent_id>/memory/`) take the audited path: they
//! skip the approval gate, are capped, and are committed into the owning
//! standalone git repo — see [`super::managed_repo`]. This is what makes
//! "save a memory as you go" a real affordance instead of a per-save
//! prompt, while keeping every save revertible.
//!
//! An identity file gets the ownership check and the audit commit but
//! **not** the approval bypass: a soul is amended in place with `Edit`, and
//! a whole-file overwrite of one is the rare, deliberate act that should
//! still meet the gate. The two halves are decided independently —
//! `accessed_resources` grants the bypass, `audit_target` records the
//! change — so refusing one does not mean forgoing the other, and a write
//! the user approved still lands in the history.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use baybo_workspace::WorkspacePaths;
use serde::Deserialize;
use serde_json::{Value, json};

use super::managed_repo::{
    ChangeKind, ManagedRoots, append_audit_line, commit_change, reject_oversized_content,
    write_managed_file,
};
use super::paths::require_absolute;
use super::permission::LivePermissionMode;
use crate::{ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

/// Named once: `name()`, rejection messages, the `Tool:` trailer of every
/// audit commit, and `baybo-context` — which tells a whole-file rewrite apart
/// from an `Edit` when it looks for the copy of a file the model is holding —
/// all have to agree.
pub const WRITE_TOOL_NAME: &str = "Write";

pub struct WriteTool {
    roots: ManagedRoots,
    permission: Arc<LivePermissionMode>,
}

impl WriteTool {
    pub fn new(workspace_paths: WorkspacePaths, permission: Arc<LivePermissionMode>) -> Self {
        Self {
            roots: ManagedRoots::new(&workspace_paths),
            permission,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Params {
    file_path: PathBuf,
    content: String,
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        WRITE_TOOL_NAME
    }

    fn description(&self) -> String {
        "Create or overwrite a file. Prefer `Edit` for existing files; only \
         use `Write` for new files or full rewrites. Parent dirs must exist."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Absolute path; relative is rejected" },
                "content":   { "type": "string", "description": "Full file content" }
            },
            "required": ["file_path", "content"]
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
        // Writes targeting `<workspace>/work` skip the approval gate to
        // match Bash's bypass for non-destructive ops inside `work/`, and
        // memory files skip it because their accountability is the audit
        // commit. Everything else (an identity file, config/, $HOME, /tmp,
        // …) keeps the prompt as a backstop — unless the operator turned the
        // gate off wholesale, which is what `permission = free` says.
        if self.permission.get().waives_approval() || self.roots.write_bypasses_approval(&path) {
            return Vec::new();
        }
        vec![ResourceAccess::WriteFile { path }]
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        require_absolute(&p.file_path, WRITE_TOOL_NAME, "file_path")?;

        // Resolved before the write, so a rejected save leaves the tree
        // exactly as it was. Unlike Edit's cap (which guards slurping an
        // existing blob), the bound that matters for a create is the size
        // of the body coming in.
        let audit_target = self
            .roots
            .audit_target(Path::new(&p.file_path), &ctx.agent_id)?;
        if audit_target.is_some() {
            reject_oversized_content(p.content.len(), &p.file_path)?;
        }

        // Read-before-write contract, but only for an overwrite: a successful
        // `stat` means the path already exists → the file must have been read
        // and be unchanged. A missing file is a fresh create, which has
        // nothing to have read, so it proceeds straight to the write.
        if let Some(tracker) = &ctx.read_tracker
            && let Ok(meta) = tokio::fs::metadata(&p.file_path).await
            && let Some(reason) = tracker
                .check(&p.file_path, crate::FileFingerprint::from_metadata(&meta))
                .rejection(&p.file_path, "overwriting it with Write")
        {
            return Err(ToolError::Execution(reason));
        }

        match &audit_target {
            // Inside `personas/`: the write itself applies whatever governs
            // that file's contents — see `write_managed_file`.
            Some(target) => {
                write_managed_file(target, &ctx.agent_id, &p.file_path, &p.content).await?
            }
            None => tokio::fs::write(&p.file_path, &p.content)
                .await
                .map_err(|e| {
                    ToolError::Execution(format!("write {}: {e}", p.file_path.display()))
                })?,
        }

        // Anchor the read baseline to the file we just wrote so an Edit that
        // follows this Write does not demand a separate Read.
        if let Some(tracker) = &ctx.read_tracker {
            tracker.record_write_from_disk(&p.file_path);
        }

        let mut text = format!(
            "wrote {} bytes to {}",
            p.content.len(),
            p.file_path.display()
        );

        if let Some(target) = audit_target {
            let outcome = commit_change(
                self.roots.personas_dir(),
                &target,
                ChangeKind::Update,
                WRITE_TOOL_NAME,
                ctx.session_id.as_str(),
            )
            .await;
            append_audit_line(&mut text, outcome);
        }

        Ok(ToolOutput::Text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::super::permission::PermissionMode;
    use super::*;
    use crate::builtin::symlinked_root;
    use baybo_model::{ChannelType, User};
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
            workspace_paths: baybo_workspace::WorkspacePaths::new("/tmp"),
            ..ToolContext::for_test()
        }
    }

    fn ctx_with_tracker(tracker: crate::ReadTracker) -> ToolContext {
        ToolContext {
            read_tracker: Some(tracker),
            ..ctx()
        }
    }

    fn tool() -> WriteTool {
        rooted(baybo_workspace::WorkspacePaths::new("/tmp"))
    }

    fn rooted(paths: WorkspacePaths) -> WriteTool {
        under(paths, PermissionMode::default())
    }

    fn under(paths: WorkspacePaths, permission: PermissionMode) -> WriteTool {
        WriteTool::new(paths, Arc::new(LivePermissionMode::new(permission)))
    }

    #[tokio::test]
    async fn creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("new.txt");
        tool()
            .execute(json!({ "file_path": p, "content": "hi" }), &ctx())
            .await
            .unwrap();
        assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "hi");
    }

    #[tokio::test]
    async fn rejects_relative_path() {
        let err = tool()
            .execute(json!({ "file_path": "rel.txt", "content": "x" }), &ctx())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParams(ref m) if m.contains("absolute")),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn new_file_needs_no_prior_read() {
        // Creating a fresh file has nothing to have read — the contract does
        // not apply even with a tracker wired.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("new.txt");
        tool()
            .execute(
                json!({ "file_path": p, "content": "hi" }),
                &ctx_with_tracker(crate::ReadTracker::default()),
            )
            .await
            .unwrap();
        assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "hi");
    }

    #[tokio::test]
    async fn rejects_overwrite_without_prior_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("exists.txt");
        tokio::fs::write(&p, "old").await.unwrap();
        let err = tool()
            .execute(
                json!({ "file_path": p, "content": "new" }),
                &ctx_with_tracker(crate::ReadTracker::default()),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Execution(ref m) if m.contains("has not been read")),
            "got: {err:?}"
        );
        // Existing content preserved on rejection.
        assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "old");
    }

    #[tokio::test]
    async fn allows_overwrite_after_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("exists.txt");
        tokio::fs::write(&p, "old").await.unwrap();
        let tracker = crate::ReadTracker::default();
        tracker.record_write_from_disk(&p);
        tool()
            .execute(
                json!({ "file_path": p, "content": "new" }),
                &ctx_with_tracker(tracker),
            )
            .await
            .unwrap();
        assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "new");
    }

    #[tokio::test]
    async fn rejects_overwrite_when_file_changed_since_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("exists.txt");
        tokio::fs::write(&p, "old").await.unwrap();
        let tracker = crate::ReadTracker::default();
        tracker.record_write_from_disk(&p);
        tokio::fs::write(&p, "changed out from under us")
            .await
            .unwrap();
        let err = tool()
            .execute(
                json!({ "file_path": p, "content": "new" }),
                &ctx_with_tracker(tracker),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Execution(ref m) if m.contains("changed on disk")),
            "got: {err:?}"
        );
    }

    #[test]
    fn the_description_is_compact() {
        let described = tool().description();
        assert!(
            described.len() <= 155,
            "description is too long: {described}"
        );
    }

    #[test]
    fn accessed_resources_skips_writefile_inside_work_dir() {
        let paths = baybo_workspace::WorkspacePaths::new("/var/baybo");
        let write = rooted(paths.clone());
        let in_work = paths.work_dir().join("scratch.txt");
        assert!(
            write
                .accessed_resources(&json!({ "file_path": in_work.to_string_lossy() }))
                .is_empty(),
            "writes inside <workspace>/work must skip approval",
        );
    }

    #[test]
    fn accessed_resources_keeps_writefile_outside_work_dir() {
        let paths = baybo_workspace::WorkspacePaths::new("/var/baybo");
        let write = rooted(paths.clone());

        let in_personas = paths.persona_identity_file(
            baybo_workspace::paths::BUILTIN_PERSONA_DIR,
            baybo_workspace::IdentityKind::Soul,
        );
        let resources =
            write.accessed_resources(&json!({ "file_path": in_personas.to_string_lossy() }));
        assert!(
            resources
                .iter()
                .any(|r| matches!(r, ResourceAccess::WriteFile { .. })),
            "writes into other workspace subtrees must still declare WriteFile",
        );

        let outside = write.accessed_resources(&json!({ "file_path": "/tmp/random.txt" }));
        assert!(
            outside
                .iter()
                .any(|r| matches!(r, ResourceAccess::WriteFile { .. })),
            "writes outside the workspace must still declare WriteFile",
        );
    }

    #[test]
    fn accessed_resources_skips_approval_for_memory_files() {
        let paths = baybo_workspace::WorkspacePaths::new("/var/baybo");
        let write = rooted(paths.clone());

        for memory_file in [
            paths.persona_memory_dir("baybo").join("cat-name.md"),
            paths.persona_memory_dir("01JSCOUT").join("cat-name.md"),
        ] {
            assert!(
                write
                    .accessed_resources(&json!({ "file_path": memory_file.to_string_lossy() }))
                    .is_empty(),
                "saving a memory must not need approval: {}",
                memory_file.display(),
            );
        }
    }

    async fn run_git_quiet(dir: &Path, args: &[&str]) {
        let status = tokio::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .await
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }

    const AGENT: &str = "baybo";

    /// A workspace whose `personas/` is an initialised git repo holding this
    /// agent's memory tree.
    async fn memory_workspace(dir: &Path) -> baybo_workspace::WorkspacePaths {
        let paths = baybo_workspace::WorkspacePaths::new(dir.to_path_buf());
        tokio::fs::create_dir_all(paths.persona_memory_dir(AGENT))
            .await
            .expect("mkdir");
        run_git_quiet(&paths.personas_dir(), &["init", "--quiet", "-b", "main"]).await;
        paths
    }

    fn ctx_for(paths: &baybo_workspace::WorkspacePaths) -> ToolContext {
        ToolContext {
            session_id: "sess-test".into(),
            agent_id: baybo_model::AgentProfileId::parse(AGENT).expect("valid id"),
            workspace_root: paths.root().to_path_buf(),
            workspace_paths: paths.clone(),
            ..ctx()
        }
    }

    #[tokio::test]
    async fn saving_a_memory_commits_it_with_the_write_tool_named() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = memory_workspace(tmp.path()).await;
        let write = rooted(paths.clone());
        // A freely-named file: the memory tier is location-keyed, with no
        // filename allowlist of the kind the identity store has.
        let fact = paths.persona_memory_dir(AGENT).join("cat-name.md");

        let out = write
            .execute(
                json!({ "file_path": fact.to_string_lossy(), "content": "Mochi.\n" }),
                &ctx_for(&paths),
            )
            .await
            .expect("write");

        let ToolOutput::Text(text) = out else {
            panic!("expected text output")
        };
        assert!(text.contains("committed to personas/"), "got: {text}");

        let log = tokio::process::Command::new("git")
            .arg("-C")
            .arg(paths.personas_dir())
            .args(["log", "-1", "--pretty=%an <%ae>%n%B"])
            .output()
            .await
            .expect("git log");
        let log = String::from_utf8_lossy(&log.stdout);
        assert!(log.contains("Baybo <baybo@local>"), "got: {log}");
        assert!(log.contains("Tool: Write"), "got: {log}");
        assert!(log.contains("Session: sess-test"), "got: {log}");
    }

    #[tokio::test]
    async fn rewriting_a_memory_with_identical_bytes_warns_about_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = memory_workspace(tmp.path()).await;
        let write = rooted(paths.clone());
        let fact = paths.persona_memory_dir(AGENT).join("cat-name.md");
        let params = json!({ "file_path": fact.to_string_lossy(), "content": "Mochi.\n" });

        write
            .execute(params.clone(), &ctx_for(&paths))
            .await
            .expect("first write");
        // `git commit` fails on an empty commit, so a byte-identical save
        // must be recognised as a no-op rather than reported as a failure.
        let out = write
            .execute(params, &ctx_for(&paths))
            .await
            .expect("second write");

        let ToolOutput::Text(text) = out else {
            panic!("expected text output")
        };
        assert!(!text.contains("commit_warning"), "got: {text}");
        assert!(!text.contains("committed to"), "nothing changed: {text}");
    }

    #[tokio::test]
    async fn overwriting_another_agents_soul_is_refused_outright() {
        // Not merely gated: `Edit` refuses it, and a whole-file overwrite is
        // the more destructive verb of the two.
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = memory_workspace(tmp.path()).await;
        let write = rooted(paths.clone());
        let theirs = paths.persona_identity_file("01JSCOUT", baybo_workspace::IdentityKind::Soul);
        tokio::fs::create_dir_all(paths.persona_dir("01JSCOUT"))
            .await
            .expect("mkdir");
        tokio::fs::write(&theirs, "their soul").await.expect("seed");

        let err = write
            .execute(
                json!({ "file_path": theirs.to_string_lossy(), "content": "mine now" }),
                &ctx_for(&paths),
            )
            .await
            .expect_err("each agent owns its own identity files");

        assert!(matches!(err, ToolError::InvalidParams(_)), "got: {err:?}");
        assert_eq!(
            tokio::fs::read_to_string(&theirs).await.expect("read"),
            "their soul"
        );
    }

    #[tokio::test]
    async fn overwriting_this_agents_own_soul_is_audited() {
        // Write keeps the approval gate on identity files, but a write the
        // user approved must still land in the history the tier exists for.
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = memory_workspace(tmp.path()).await;
        let write = rooted(paths.clone());
        let mine = paths.persona_identity_file(AGENT, baybo_workspace::IdentityKind::Soul);
        tokio::fs::create_dir_all(paths.persona_dir(AGENT))
            .await
            .expect("mkdir");

        let out = write
            .execute(
                json!({ "file_path": mine.to_string_lossy(), "content": "# Soul\n\nRewritten.\n" }),
                &ctx_for(&paths),
            )
            .await
            .expect("an agent may overwrite its own soul");

        let ToolOutput::Text(text) = out else {
            panic!("expected text output")
        };
        assert!(text.contains("committed to personas/"), "got: {text}");

        // …and the gate is still declared for it, unlike a memory file.
        assert!(
            write
                .accessed_resources(&json!({ "file_path": mine.to_string_lossy() }))
                .iter()
                .any(|r| matches!(r, ResourceAccess::WriteFile { .. })),
            "a soul overwrite must still meet the approval gate"
        );
    }

    #[tokio::test]
    async fn overwriting_an_identity_file_cannot_rename_a_project_agent() {
        // `Edit` is not the only door into that `Name:` line — a whole-file
        // overwrite reaches it too, and it is the more destructive verb.
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = memory_workspace(tmp.path()).await;
        let write = rooted(paths.clone());
        let hired = "project-01JDEV";
        let named = "# Who Am I?\n\n* **Name:** Dev\n";
        let mine = paths.persona_identity_file(hired, baybo_workspace::IdentityKind::Identity);
        tokio::fs::create_dir_all(paths.persona_dir(hired))
            .await
            .expect("mkdir");
        tokio::fs::write(&mine, named).await.expect("seed");
        let ctx = ToolContext {
            agent_id: baybo_model::AgentProfileId::parse(hired).expect("valid id"),
            ..ctx_for(&paths)
        };

        let err = write
            .execute(
                json!({
                    "file_path": mine.to_string_lossy(),
                    "content": "# Who Am I?\n\n* **Name:** Aster\n",
                }),
                &ctx,
            )
            .await
            .expect_err("the @handle its board uses came from that name");
        assert!(
            matches!(err, ToolError::InvalidParams(ref m) if m.contains("@handle")),
            "got: {err:?}"
        );
        assert_eq!(
            tokio::fs::read_to_string(&mine).await.expect("read"),
            named,
            "a refused write leaves the file alone"
        );

        // Rewriting everything around the name is what the file is for.
        write
            .execute(
                json!({
                    "file_path": mine.to_string_lossy(),
                    "content": "# Who Am I?\n\n* **Name:** Dev\n* **Vibe:** warm\n",
                }),
                &ctx,
            )
            .await
            .expect("only the name is fixed");
    }

    #[tokio::test]
    async fn a_memory_body_over_the_cap_is_refused_before_the_write() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = memory_workspace(tmp.path()).await;
        let write = rooted(paths.clone());
        let fact = paths.persona_memory_dir(AGENT).join("huge.md");
        let body = "x".repeat((super::super::managed_repo::MAX_MANAGED_FILE_BYTES + 1) as usize);

        let err = write
            .execute(
                json!({ "file_path": fact.to_string_lossy(), "content": body }),
                &ctx_for(&paths),
            )
            .await
            .expect_err("must refuse");

        assert!(matches!(err, ToolError::Execution(_)), "got: {err:?}");
        assert!(!fact.exists(), "the refused write must not land");
    }

    /// The checkout an issue run is told to work in, named the way its own
    /// brief names it. `ManagedRoots` resolves its roots, so before the path
    /// was resolved on the same terms this declared `WriteFile` and every
    /// file the run touched raised a separate prompt.
    #[test]
    fn a_checkout_under_a_symlinked_root_still_skips_the_gate() {
        let (_tmp, real) = symlinked_root::canonical_tempdir();
        let linked = symlinked_root::workspace(&real);
        let write = rooted(linked.clone());

        let checkout = linked
            .work_dir()
            .join(".worktrees")
            .join("01M1E94QY306MFQSFH1DQX2V78")
            .join("1")
            .join("src")
            .join("types.ts");
        assert!(
            write
                .accessed_resources(&json!({ "file_path": checkout.to_string_lossy() }))
                .is_empty(),
            "{} is inside work/ whichever spelling names it",
            checkout.display(),
        );
    }

    /// `permission = free` is the operator saying the gate is off. Bash has
    /// always read it that way; a file write is not the exception.
    #[test]
    fn permission_free_waives_the_gate_outside_work_too() {
        let paths = baybo_workspace::WorkspacePaths::new("/var/baybo");
        for (permission, waived) in [
            (PermissionMode::Free, true),
            (PermissionMode::Auto, false),
            (PermissionMode::Manual, false),
        ] {
            let declared = under(paths.clone(), permission)
                .accessed_resources(&json!({ "file_path": "/tmp/random.txt" }));
            assert_eq!(declared.is_empty(), waived, "under {permission:?}");
        }
    }
}
