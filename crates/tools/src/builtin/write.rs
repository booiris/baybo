//! `Write` — create or overwrite a file with the given contents.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use baybo_workspace::{WorkspacePaths, absolutise};
use serde::Deserialize;
use serde_json::{Value, json};

use super::paths::require_absolute;
use crate::{ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

pub struct WriteTool {
    work_dir: PathBuf,
}

impl WriteTool {
    pub fn new(workspace_paths: WorkspacePaths) -> Self {
        Self {
            work_dir: absolutise(&workspace_paths.work_dir()),
        }
    }

    fn is_inside_work_dir(&self, file_path: &Path) -> bool {
        file_path.is_absolute() && file_path.starts_with(&self.work_dir)
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
        "Write"
    }

    fn description(&self) -> String {
        "Create or overwrite a file with the provided content. \
         Always use this instead of Bash commands like echo with \
         redirection or cat with heredoc. Prefer `Edit` for modifying \
         existing files — only use `Write` for new files or complete \
         rewrites. Overwriting a file that already exists requires you to \
         have Read it first (and it must be unchanged since); creating a new \
         file does not. Parent directories must already exist.\n\n\
         PATHS: `file_path` MUST be an absolute filesystem path. Relative \
         paths are rejected."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Absolute path of the file to write" },
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
        // match Bash's bypass for non-destructive ops inside `work/`.
        // Everything else (profile/, config/, $HOME, /tmp, …) keeps
        // the prompt as a backstop.
        if self.is_inside_work_dir(&path) {
            return Vec::new();
        }
        vec![ResourceAccess::WriteFile { path }]
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        require_absolute(&p.file_path, "Write", "file_path")?;

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

        tokio::fs::write(&p.file_path, &p.content)
            .await
            .map_err(|e| ToolError::Execution(format!("write {}: {e}", p.file_path.display())))?;

        // Anchor the read baseline to the file we just wrote so an Edit that
        // follows this Write does not demand a separate Read.
        if let Some(tracker) = &ctx.read_tracker {
            tracker.record_write_from_disk(&p.file_path);
        }

        Ok(ToolOutput::Text(format!(
            "wrote {} bytes to {}",
            p.content.len(),
            p.file_path.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        WriteTool::new(baybo_workspace::WorkspacePaths::new("/tmp"))
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
    fn accessed_resources_skips_writefile_inside_work_dir() {
        let paths = baybo_workspace::WorkspacePaths::new("/var/baybo");
        let write = WriteTool::new(paths.clone());
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
        let write = WriteTool::new(paths.clone());

        let in_profile = paths.profile_dir().join("SOUL.md");
        let resources =
            write.accessed_resources(&json!({ "file_path": in_profile.to_string_lossy() }));
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
}
