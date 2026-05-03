use std::path::PathBuf;
use std::sync::LazyLock;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use super::paths::require_absolute;
use crate::{ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 50_00;
const MAX_LINE_BYTES: usize = 2000;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_FILE_MIB: u64 = MAX_FILE_BYTES / 1024 / 1024;

static DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Read the contents of a file from the local filesystem. \
         Always use this instead of Bash commands like cat, head, or tail. \
         Supports optional `offset` (1-based starting line) and `limit` \
         (max lines, default {DEFAULT_LIMIT}, capped at {MAX_LIMIT}). Long \
         individual lines are truncated to {MAX_LINE_BYTES} bytes (at a \
         UTF-8 char boundary). Files larger than {MAX_FILE_MIB} MiB are \
         only scanned for the first {MAX_FILE_MIB} MiB of content. Output \
         is formatted with line numbers for easy reference.\n\n\
         PATHS: `file_path` MUST be an absolute filesystem path. Relative \
         paths are rejected."
    )
});

static LIMIT_DESC: LazyLock<String> = LazyLock::new(|| {
    format!("Maximum number of lines to read (default {DEFAULT_LIMIT}, max {MAX_LIMIT})")
});

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
        &DESCRIPTION
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Absolute path to the file" },
                "offset": { "type": "integer", "minimum": 1, "description": "Line number to start reading from (1-based)" },
                "limit": { "type": "integer", "minimum": 1, "description": &*LIMIT_DESC }
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

        require_absolute(&p.file_path, "Read", "file_path")?;

        if aura_security::is_sensitive_path(&p.file_path) {
            return Err(ToolError::Execution(format!(
                "refused to read sensitive path {} — credential-bearing files are blocked by security policy",
                p.file_path.display()
            )));
        }

        let start = p.offset.unwrap_or(1).saturating_sub(1);
        let limit = p.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        let end = start.saturating_add(limit);

        let total_size = tokio::fs::metadata(&p.file_path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);

        let file = tokio::fs::File::open(&p.file_path)
            .await
            .map_err(|e| ToolError::Execution(format!("read {}: {e}", p.file_path.display())))?;
        let mut reader = BufReader::new(file.take(MAX_FILE_BYTES)).lines();

        let mut out = String::new();
        let mut idx = 0usize;
        while let Some(line) = reader
            .next_line()
            .await
            .map_err(|e| ToolError::Execution(format!("read {}: {e}", p.file_path.display())))?
        {
            if idx >= end {
                break;
            }
            if idx >= start {
                let cut = line.floor_char_boundary(MAX_LINE_BYTES);
                if cut < line.len() {
                    out.push_str(&format!("{:>6}\t{}… [truncated]\n", idx + 1, &line[..cut]));
                } else {
                    out.push_str(&format!("{:>6}\t{}\n", idx + 1, line));
                }
            }
            idx += 1;
        }

        if total_size > MAX_FILE_BYTES {
            out.push_str(&format!(
                "… [Read scanned only the first {MAX_FILE_MIB} MiB; file is {total_size} bytes total]\n"
            ));
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
                channel: ChannelType::tui(),
                bot_id: None,
            },
            timeout: Duration::from_secs(5),
            cancellation_token: CancellationToken::new(),
            workspace_root: std::path::PathBuf::from("/tmp"),
            sandbox: None,
            approval: None,
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
    async fn refuses_sensitive_path() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        tokio::fs::create_dir(&ssh).await.unwrap();
        let key = ssh.join("id_rsa");
        tokio::fs::write(&key, "FAKE KEY DATA").await.unwrap();
        let err = ReadTool
            .execute(json!({ "file_path": key }), &ctx())
            .await
            .unwrap_err();
        let msg = format!("{:?}", err);
        assert!(
            msg.contains("sensitive path") || msg.contains("credential-bearing"),
            "got: {msg}"
        );
    }

    #[tokio::test]
    async fn rejects_relative_path() {
        let err = ReadTool
            .execute(json!({ "file_path": "relative.txt" }), &ctx())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParams(ref m) if m.contains("absolute")),
            "got: {err:?}"
        );
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

    #[tokio::test]
    async fn truncates_long_line_at_utf8_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("emoji.txt");
        // 600 octopodes = 2400 bytes — over MAX_LINE_BYTES=2000.
        // An unsafe byte slice would land mid-codepoint and panic.
        let line: String = "🐙".repeat(600);
        tokio::fs::write(&p, &line).await.unwrap();
        let out = ReadTool
            .execute(json!({ "file_path": p }), &ctx())
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else {
            panic!();
        };
        assert!(s.contains("truncated"), "expected truncation marker: {s}");
    }
}
