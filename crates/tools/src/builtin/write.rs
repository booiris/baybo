//! `Write` — create or overwrite a file with the given contents.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use aura_workspace::{WorkspacePaths, absolutise};
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
         rewrites. Parent directories must already exist.\n\n\
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

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        require_absolute(&p.file_path, "Write", "file_path")?;

        tokio::fs::write(&p.file_path, &p.content)
            .await
            .map_err(|e| ToolError::Execution(format!("write {}: {e}", p.file_path.display())))?;

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
    use aura_model::{ChannelType, User};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        ToolContext {
            session_id: "t".into(),
            job_id: aura_model::JobId::default(),
            span_id: aura_model::SpanId::default(),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            timeout: Duration::from_secs(5),
            cancellation_token: CancellationToken::new(),
            workspace_root: std::path::PathBuf::from("/tmp"),
            workspace_paths: aura_workspace::WorkspacePaths::new("/tmp"),
            sandbox: None,
            approval: None,
            notifier: None,
            events: crate::noop_event_sink(),
            llm: None,
            secrets: None,
            virtual_reads: None,
            background_jobs: None,
            background_control: None,
        }
    }

    fn tool() -> WriteTool {
        WriteTool::new(aura_workspace::WorkspacePaths::new("/tmp"))
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

    #[test]
    fn accessed_resources_skips_writefile_inside_work_dir() {
        let paths = aura_workspace::WorkspacePaths::new("/var/aura");
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
        let paths = aura_workspace::WorkspacePaths::new("/var/aura");
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
