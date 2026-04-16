//! `Read` — read file contents with optional line range.

use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

const DEFAULT_LIMIT: usize = 2000;
const MAX_LINE_LEN: usize = 2000;

pub struct ReadTool;

#[derive(Debug, Deserialize)]
struct Params {
    file_path: PathBuf,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read the contents of a file from the local filesystem. Supports \
         optional `offset` (1-based starting line) and `limit` (max lines). \
         Long individual lines are truncated to 2000 characters."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Absolute path to the file" },
                "offset": { "type": "integer", "minimum": 1, "description": "Line number to start reading from (1-based)" },
                "limit": { "type": "integer", "minimum": 1, "description": "Maximum number of lines to read (default 2000)" }
            },
            "required": ["file_path"]
        })
    }

    fn accessed_resources(&self, params: &Value) -> Vec<ResourceAccess> {
        params
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|s| {
                vec![ResourceAccess::ReadFile {
                    path: PathBuf::from(s),
                }]
            })
            .unwrap_or_default()
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        let contents = tokio::fs::read_to_string(&p.file_path)
            .await
            .map_err(|e| ToolError::Execution(format!("read {}: {e}", p.file_path.display())))?;

        let start = p.offset.unwrap_or(1).saturating_sub(1);
        let limit = p.limit.unwrap_or(DEFAULT_LIMIT);

        let mut out = String::new();
        for (i, line) in contents.lines().enumerate().skip(start).take(limit) {
            let truncated = if line.len() > MAX_LINE_LEN {
                format!("{}… [truncated]", &line[..MAX_LINE_LEN])
            } else {
                line.to_string()
            };
            out.push_str(&format!("{:>6}\t{}\n", i + 1, truncated));
        }

        if out.is_empty() {
            out.push_str("(file is empty or range out of bounds)");
        }

        Ok(ToolOutput::Text(out))
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
            session_id: "test".into(),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::Tui,
            },
            timeout: Duration::from_secs(5),
            cancellation_token: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn reads_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        tokio::fs::write(&p, "one\ntwo\nthree\n").await.unwrap();
        let out = ReadTool
            .execute(json!({ "file_path": p }), &ctx())
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else {
            panic!();
        };
        assert!(s.contains("one"));
        assert!(s.contains("three"));
    }

    #[tokio::test]
    async fn respects_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("b.txt");
        tokio::fs::write(&p, "l1\nl2\nl3\nl4\n").await.unwrap();
        let out = ReadTool
            .execute(json!({ "file_path": p, "offset": 2, "limit": 2 }), &ctx())
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else {
            panic!();
        };
        assert!(s.contains("l2"));
        assert!(s.contains("l3"));
        assert!(!s.contains("l1"));
        assert!(!s.contains("l4"));
    }
}
