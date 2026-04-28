//! `Edit` — targeted string replacement inside an existing file.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::paths::require_absolute;
use crate::{ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

pub struct EditTool;

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

    fn description(&self) -> &str {
        "Perform targeted string replacement inside a file. Always use \
         this instead of Bash commands like sed or awk for editing files. \
         Replace `old_string` with `new_string`; when `replace_all` is \
         false (default), `old_string` must appear exactly once — otherwise \
         the tool fails without touching the file. Provide enough surrounding \
         context in `old_string` to ensure a unique match.\n\n\
         PATHS: `file_path` MUST be an absolute filesystem path. Relative \
         paths are rejected."
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

    fn accessed_resources(&self, params: &Value) -> Vec<ResourceAccess> {
        params
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|s| {
                let p = PathBuf::from(s);
                vec![
                    ResourceAccess::ReadFile { path: p.clone() },
                    ResourceAccess::WriteFile { path: p },
                ]
            })
            .unwrap_or_default()
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        require_absolute(&p.file_path, "Edit", "file_path")?;

        if p.old_string == p.new_string {
            return Err(ToolError::InvalidParams(
                "old_string and new_string are identical".into(),
            ));
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

        let replaced = if p.replace_all { matches } else { 1 };
        Ok(ToolOutput::Text(format!(
            "replaced {replaced} occurrence(s) in {}",
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
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            timeout: Duration::from_secs(5),
            cancellation_token: CancellationToken::new(),
            workspace_root: std::path::PathBuf::from("/tmp"),
            sandbox: None,
            approval: None,
        }
    }

    #[tokio::test]
    async fn single_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        tokio::fs::write(&p, "alpha beta gamma").await.unwrap();
        EditTool
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
        let err = EditTool
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
        let err = EditTool
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
        EditTool
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
}
