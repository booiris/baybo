//! `Write` — create or overwrite a file with the given contents.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::paths::require_absolute;
use crate::{ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

pub struct WriteTool;

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

    fn description(&self) -> &str {
        "Create or overwrite a file with the provided content. \
         Always use this instead of Bash commands like echo with \
         redirection or cat with heredoc. Prefer `Edit` for modifying \
         existing files — only use `Write` for new files or complete \
         rewrites. Parent directories must already exist.\n\n\
         PATHS: `file_path` MUST be an absolute filesystem path. Relative \
         paths are rejected."
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

    fn accessed_resources(&self, params: &Value) -> Vec<ResourceAccess> {
        params
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|s| {
                vec![ResourceAccess::WriteFile {
                    path: PathBuf::from(s),
                }]
            })
            .unwrap_or_default()
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
        }
    }

    #[tokio::test]
    async fn creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("new.txt");
        WriteTool
            .execute(json!({ "file_path": p, "content": "hi" }), &ctx())
            .await
            .unwrap();
        assert_eq!(tokio::fs::read_to_string(&p).await.unwrap(), "hi");
    }

    #[tokio::test]
    async fn rejects_relative_path() {
        let err = WriteTool
            .execute(json!({ "file_path": "rel.txt", "content": "x" }), &ctx())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParams(ref m) if m.contains("absolute")),
            "got: {err:?}"
        );
    }
}
