use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use super::paths::require_absolute;
use crate::{
    ResourceAccess, Tool, ToolConcurrency, ToolContext, ToolError, ToolOutput, VirtualReadAccess,
};

/// Canonical name of the file-reading builtin. A const so `name()`, the
/// `require_absolute` label, and the read-before-write tracker's transcript
/// reconstruction (`ReadTracker::rebuild_from_messages`) share one source of
/// truth for the literal. Re-exported at the crate root as
/// [`crate::READ_TOOL_NAME`].
pub const READ_TOOL_NAME: &str = "Read";

const DEFAULT_LIMIT: usize = 800;
const MAX_LIMIT: usize = 50_00;
const MAX_LINE_BYTES: usize = 2000;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_FILE_MIB: u64 = MAX_FILE_BYTES / 1024 / 1024;

/// Format one line of `Read`-style output: a 1-based line number, a tab,
/// then the line truncated to [`MAX_LINE_BYTES`] at a UTF-8 boundary (with a
/// `… [truncated]` marker when cut). Used by both the filesystem read loop
/// and the virtual-file path below.
///
/// The number is not padded to a column. Alignment is for a human scanning a
/// page; the model reads the tab, and a gutter of spaces on every line of
/// every read is the single largest avoidable share of what `Read` spends.
fn format_numbered_line(line_no: usize, line: &str) -> String {
    let cut = line.floor_char_boundary(MAX_LINE_BYTES);
    if cut < line.len() {
        format!("{}\t{}… [truncated]\n", line_no, &line[..cut])
    } else {
        format!("{}\t{}\n", line_no, line)
    }
}

/// Turn a mid-read failure into a message that distinguishes the very
/// different things it can mean.
///
/// The failing call is `next_line`, so a binary file surfaces as a *decode*
/// verdict — "stream did not contain valid UTF-8" — formatted identically to
/// the `File::open` ENOENT above it. An agent that guessed a path and got
/// this back cannot tell "wrong path" from "right path, wrong tool", which is
/// exactly the distinction it was probing for. The file has already been
/// stat'd by this point, so saying that it is there and how big costs
/// nothing.
///
/// The size gate matters: past [`MAX_FILE_BYTES`] the reader is a `take`, so
/// a decode failure is equally likely to be the cut landing mid-codepoint in
/// a perfectly good UTF-8 file. Calling that one "binary" would send the
/// model to Bash for a file it could have read with a narrower window.
///
/// Neither message routes the model to `GetBlob`. It is keyed by `blob_id`
/// while the caller arrived here holding a path, with no way back; and since
/// `GetBlob` answers *with* payload paths, the likeliest arrival is a model
/// that called it and then `Read` the image it got — so naming it would be
/// advice already taken.
fn decode_failure(path: &Path, total_size: u64, e: &std::io::Error) -> ToolError {
    if e.kind() != std::io::ErrorKind::InvalidData {
        return ToolError::Execution(format!("read {}: {e}", path.display()));
    }
    if total_size > MAX_FILE_BYTES {
        return ToolError::Execution(format!(
            "read {}: {total_size} bytes; the first {MAX_FILE_MIB} MiB are not valid UTF-8 — \
             either the file is binary, or the {MAX_FILE_MIB} MiB cut split a multi-byte \
             character. Retry with a `limit` that stops before the cut, or use Bash if it is \
             a binary payload.",
            path.display()
        ));
    }
    ToolError::Execution(format!(
        "read {}: exists ({total_size} bytes) but is not UTF-8 text — Read returns text \
         only. Pass the path to whatever consumes the file (via Bash: `file`, `xxd`, \
         ffmpeg, …).",
        path.display()
    ))
}

/// The exclusive last line index a `Read` with these params will emit —
/// the same offset/limit defaulting [`paginate_numbered`] applies
/// (default [`DEFAULT_LIMIT`], capped at [`MAX_LIMIT`]). Exported so a
/// [`crate::VirtualReadResolver`] can stop materialising content at the
/// window's end without re-deriving the paginator's defaults.
pub fn paginate_end_line(offset: Option<usize>, limit: Option<usize>) -> usize {
    let start = offset.unwrap_or(1).saturating_sub(1);
    start.saturating_add(limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT))
}

/// Render an in-memory string as `Read`-style numbered output, honoring the
/// same 1-based `offset` / `limit` (default [`DEFAULT_LIMIT`], capped at
/// [`MAX_LIMIT`]) and per-line byte cap as a real read. Exported for
/// [`crate::VirtualReadResolver`] implementations, which paginate their own
/// content so they can stop materialising it at the window's end.
pub fn paginate_numbered(content: &str, offset: Option<usize>, limit: Option<usize>) -> String {
    let start = offset.unwrap_or(1).saturating_sub(1);
    let end = paginate_end_line(offset, limit);
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

    fn output_source(&self) -> crate::OutputSource {
        crate::OutputSource::DeclaredFiles
    }

    /// Read-only (filesystem or virtual transcript); mutates no shared
    /// state, so parallel reads within a turn cannot race.
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
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
            let window = crate::VirtualReadWindow {
                offset: p.offset,
                limit: p.limit,
            };
            // The resolver returns finished `Read`-style output for the
            // window — pagination happens at the source so the whole
            // virtual file is never materialised per page.
            match resolver.resolve(&p.file_path, &access, &window).await {
                Some(Ok(content)) => return Ok(ToolOutput::Text(content)),
                Some(Err(reason)) => return Err(ToolError::Execution(reason)),
                None => {}
            }
        }

        if baybo_security::is_sensitive_path(&p.file_path) {
            return Err(ToolError::Execution(format!(
                "refused to read sensitive path {} — credential-bearing files are blocked by security policy",
                p.file_path.display()
            )));
        }

        let start = p.offset.unwrap_or(1).saturating_sub(1);
        let limit = p.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        let end = start.saturating_add(limit);

        let meta = tokio::fs::metadata(&p.file_path).await.ok();
        let total_size = meta.as_ref().map(|m| m.len()).unwrap_or(0);

        let file = tokio::fs::File::open(&p.file_path)
            .await
            .map_err(|e| ToolError::Execution(format!("read {}: {e}", p.file_path.display())))?;
        let mut reader = BufReader::new(file.take(MAX_FILE_BYTES)).lines();

        let mut out = String::new();
        let mut idx = 0usize;
        while let Some(line) = reader
            .next_line()
            .await
            .map_err(|e| decode_failure(&p.file_path, total_size, &e))?
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

        // Record the read so `Edit`/`Write` can enforce the read-before-write
        // contract. Anchored to the metadata captured before the read: if the
        // file changes during the read, the recorded mtime stays "older" than
        // the post-change file, so a later edit is forced to re-read — the
        // safe direction. No-op when no tracker is wired (system passes,
        // argv-mode, tests).
        if let (Some(tracker), Some(meta)) = (&ctx.read_tracker, &meta) {
            tracker.record_read(&p.file_path, crate::FileFingerprint::from_metadata(meta));
        }

        Ok(ToolOutput::Text(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::{ChannelType, User};
    use std::sync::Arc;
    use std::time::Duration;

    fn ctx() -> ToolContext {
        ToolContext {
            session_id: "test".into(),
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
            window: &crate::VirtualReadWindow,
        ) -> Option<Result<String, String>> {
            match self {
                StubResolver::Unclaimed => None,
                // Honour the contract: the resolver ships finished
                // `Read`-style output for the window.
                StubResolver::Content(s) => {
                    Some(Ok(paginate_numbered(s, window.offset, window.limit)))
                }
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

    #[tokio::test]
    async fn a_binary_file_says_it_was_found() {
        let dir = tempfile::tempdir().unwrap();
        let found = dir.path().join("photo.jpg");
        tokio::fs::write(&found, [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10])
            .await
            .unwrap();

        let binary = ReadTool
            .execute(json!({ "file_path": found }), &ctx())
            .await
            .unwrap_err()
            .to_string();
        let missing = ReadTool
            .execute(
                json!({ "file_path": dir.path().join("absent.jpg") }),
                &ctx(),
            )
            .await
            .unwrap_err()
            .to_string();

        // The whole point: the two must not read alike. A guessed path that
        // was RIGHT has to be distinguishable from one that was wrong.
        assert!(binary.contains("exists (6 bytes)"), "{binary}");
        assert!(binary.contains("not UTF-8 text"), "{binary}");
        assert!(!missing.contains("exists"), "{missing}");
    }

    #[test]
    fn progress_label_previews_file_path() {
        assert_eq!(
            ReadTool
                .progress_label(&json!({
                    "file_path": "/data/baybo/crates/tools/src/builtin/read.rs"
                }))
                .as_deref(),
            Some("/data/baybo/crates/tools/src/builtin/read.rs"),
        );
    }

    #[test]
    fn paginate_numbers_lines_and_honors_offset_limit() {
        let out = paginate_numbered("l1\nl2\nl3\nl4", Some(2), Some(2));
        assert!(out.contains("2\tl2"));
        assert!(out.contains("3\tl3"));
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
        assert!(s.contains("1\tv1"));
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
    async fn records_fingerprint_into_tracker() {
        // A real read records the file's fingerprint so a later Edit/Write
        // can see it. A virtual read must NOT (no on-disk backing).
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        tokio::fs::write(&p, "hello").await.unwrap();
        let tracker = crate::ReadTracker::default();
        let ctx = ToolContext {
            read_tracker: Some(tracker.clone()),
            ..ctx()
        };
        ReadTool
            .execute(json!({ "file_path": p }), &ctx)
            .await
            .unwrap();
        // The read is staged (visible via `get`); a same-response check still
        // reports NeverRead until a response boundary promotes it.
        let current = crate::FileFingerprint::from_metadata(&std::fs::metadata(&p).unwrap());
        assert_eq!(tracker.get(&p), Some(current));
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
