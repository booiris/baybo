use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use super::paths::require_absolute;
use crate::{ResourceAccess, Tool, ToolContext, ToolError, ToolOutput, VirtualReadAccess};

/// Canonical name of the file-reading builtin. A const so `name()` and the
/// `require_absolute` label share one source of truth for the literal.
const READ_TOOL_NAME: &str = "Read";

const DEFAULT_LIMIT: usize = 800;
const MAX_LIMIT: usize = 50_00;
const MAX_LINE_BYTES: usize = 2000;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_FILE_MIB: u64 = MAX_FILE_BYTES / 1024 / 1024;

/// Format one line of `Read`-style output: a right-aligned 1-based line
/// number, a tab, then the line truncated to [`MAX_LINE_BYTES`] at a UTF-8
/// boundary (with a `… [truncated]` marker when cut). Used by both the
/// filesystem read loop and the virtual-file path below.
fn format_numbered_line(line_no: usize, line: &str) -> String {
    let cut = line.floor_char_boundary(MAX_LINE_BYTES);
    if cut < line.len() {
        format!("{:>6}\t{}… [truncated]\n", line_no, &line[..cut])
    } else {
        format!("{:>6}\t{}\n", line_no, line)
    }
}

/// Render an in-memory string as `Read`-style numbered output, honoring the
/// same 1-based `offset` / `limit` (default [`DEFAULT_LIMIT`], capped at
/// [`MAX_LIMIT`]) and per-line byte cap as a real read. Used by the
/// virtual-file path in [`ReadTool::execute`] to present provider content
/// identically to a file.
fn paginate_numbered(content: &str, offset: Option<usize>, limit: Option<usize>) -> String {
    let start = offset.unwrap_or(1).saturating_sub(1);
    let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let end = start.saturating_add(limit);
    let mut out = String::new();
    for (idx, line) in content.lines().enumerate() {
        if idx >= end {
            break;
        }
        if idx >= start {
            out.push_str(&format_numbered_line(idx + 1, line));
        }
    }
    if out.is_empty() {
        out.push_str("(empty or range out of bounds)");
    }
    out
}

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
        READ_TOOL_NAME
    }

    fn description(&self) -> String {
        DESCRIPTION.to_string()
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

    fn progress_label(&self, params: &Value) -> Option<String> {
        params
            .get("file_path")
            .and_then(Value::as_str)
            .map(|s| crate::progress::preview_path(Path::new(s)))
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

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        require_absolute(&p.file_path, READ_TOOL_NAME, "file_path")?;

        // A virtual read (no on-disk backing — e.g. the session transcript,
        // served from the store) is resolved before any filesystem access. The
        // resolver self-enforces access control; `None` means this path isn't
        // virtual, so the real read proceeds.
        if let Some(resolver) = &ctx.virtual_reads {
            let access = VirtualReadAccess {
                session_id: &ctx.session_id,
                user: &ctx.user,
            };
            match resolver.resolve(&p.file_path, &access).await {
                Some(Ok(content)) => {
                    return Ok(ToolOutput::Text(paginate_numbered(
                        &content, p.offset, p.limit,
                    )));
                }
                Some(Err(reason)) => return Err(ToolError::Execution(reason)),
                None => {}
            }
        }

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
                out.push_str(&format_numbered_line(idx + 1, &line));
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
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        ToolContext {
            session_id: "test".into(),
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
        }
    }

    fn ctx_with(resolver: Arc<dyn crate::VirtualReadResolver>) -> ToolContext {
        ToolContext {
            virtual_reads: Some(resolver),
            ..ctx()
        }
    }

    enum StubResolver {
        Unclaimed,
        Content(String),
        Denied,
    }

    #[async_trait]
    impl crate::VirtualReadResolver for StubResolver {
        async fn resolve(
            &self,
            _path: &Path,
            _access: &VirtualReadAccess<'_>,
        ) -> Option<Result<String, String>> {
            match self {
                StubResolver::Unclaimed => None,
                StubResolver::Content(s) => Some(Ok(s.clone())),
                StubResolver::Denied => Some(Err("not yours".into())),
            }
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

    #[test]
    fn progress_label_previews_file_path() {
        assert_eq!(
            ReadTool
                .progress_label(&json!({
                    "file_path": "/data/aura/crates/tools/src/builtin/read.rs"
                }))
                .as_deref(),
            Some("/data/aura/crates/tools/src/builtin/read.rs"),
        );
    }

    #[test]
    fn paginate_numbers_lines_and_honors_offset_limit() {
        let out = paginate_numbered("l1\nl2\nl3\nl4", Some(2), Some(2));
        assert!(out.contains("     2\tl2"));
        assert!(out.contains("     3\tl3"));
        assert!(!out.contains("l1"));
        assert!(!out.contains("l4"));
    }

    #[test]
    fn paginate_truncates_long_line_and_reports_empty_range() {
        let long = "x".repeat(MAX_LINE_BYTES + 50);
        assert!(paginate_numbered(&long, None, None).contains("… [truncated]"));
        assert_eq!(
            paginate_numbered("a\nb", Some(99), Some(1)),
            "(empty or range out of bounds)"
        );
    }

    #[tokio::test]
    async fn virtual_resolver_serves_before_filesystem() {
        // The path does not exist on disk, but the resolver claims it — Read
        // returns the resolved content, paginated like a real file.
        let out = ReadTool
            .execute(
                json!({ "file_path": "/no/such/file", "limit": 1 }),
                &ctx_with(Arc::new(StubResolver::Content("v1\nv2".into()))),
            )
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else { panic!() };
        assert!(s.contains("     1\tv1"));
        assert!(
            !s.contains("v2"),
            "limit=1 must stop before the second line"
        );
    }

    #[tokio::test]
    async fn unclaimed_resolver_falls_through_to_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("real.txt");
        tokio::fs::write(&p, "from disk\n").await.unwrap();
        let out = ReadTool
            .execute(
                json!({ "file_path": p }),
                &ctx_with(Arc::new(StubResolver::Unclaimed)),
            )
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else { panic!() };
        assert!(s.contains("from disk"));
    }

    #[tokio::test]
    async fn denied_resolver_surfaces_error() {
        let err = ReadTool
            .execute(
                json!({ "file_path": "/whatever" }),
                &ctx_with(Arc::new(StubResolver::Denied)),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(ref m) if m.contains("not yours")));
    }
}
