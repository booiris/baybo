//! `Grep` — regex content search via the `rg` (ripgrep) binary.
//!
//! Each call shells out to `rg` and parses an unambiguous output
//! format: `--json` for `content` mode and `--null` for the path-only
//! modes. Sensitive paths (SSH/AWS/GPG configs, `.env`, `/etc/shadow`,
//! …) are filtered using the full structured path **before** any match
//! line is included in the result, so their contents never leak.
//!
//! Security note: we deliberately avoid parsing rg's default text
//! format (`path:line:match`) because Unix paths may themselves contain
//! `:` (or even `\n`), and a `split_once(':')` on a path like
//! `/tmp/work:copy/.env:1:SECRET` would mis-classify the path as the
//! non-sensitive `/tmp/work` and leak the secret line.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::{Value, json};

use super::paths::require_absolute;
use super::rg;
use crate::{ResourceAccess, Tool, ToolConcurrency, ToolContext, ToolError, ToolOutput};

const MAX_HITS: usize = 500;
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_FILE_MIB: u64 = MAX_FILE_BYTES / 1024 / 1024;
/// Budget for the rows we return, counted after the sensitive-path
/// filter.
const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_OUTPUT_MIB: usize = MAX_OUTPUT_BYTES / 1024 / 1024;
/// Longest single rg record we buffer. Path-only records are far below
/// it; a `--json` match record over it — a huge line, or one whose many
/// submatches each repeat their text — is skipped and reported as omitted.
const MAX_RECORD_BYTES: usize = 4 * 1024 * 1024;
/// Also what a search whose every hit sat under a sensitive path returns:
/// telling the two apart would let a caller probe a secret file's
/// contents one pattern at a time.
const NO_MATCHES_MESSAGE: &str = "No matches found. Sensitive paths are excluded from Grep \
     results; ignored, binary, and oversized files are not searched.";

static DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Search file contents with a regular expression. Sensitive paths \
         (.env, SSH/AWS keys, …) are filtered out of the results. Skips \
         files excluded by .gitignore and files over {MAX_FILE_MIB} MiB."
    )
});

pub struct GrepTool {
    process_manager: Arc<baybo_process::ProcessManager>,
}

impl GrepTool {
    pub fn new(process_manager: Arc<baybo_process::ProcessManager>) -> Self {
        Self { process_manager }
    }

    #[cfg(test)]
    fn for_test() -> Self {
        Self::new(baybo_process::ProcessManager::transient())
    }
}

#[derive(Debug, Deserialize)]
struct Params {
    pattern: String,
    path: PathBuf,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    output_mode: OutputMode,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OutputMode {
    #[default]
    FilesWithMatches,
    Content,
    Count,
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    /// Read-only content search — safe to run concurrently.
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn description(&self) -> String {
        DESCRIPTION.to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern":          { "type": "string", "description": "Rust-flavor regex" },
                "path":             { "type": "string", "description": "Absolute file or directory" },
                "glob":             { "type": "string", "description": "Filename glob (e.g. `*.rs`). Empty = no filter." },
                "case_insensitive": { "type": "boolean", "default": false },
                "output_mode":      {
                    "type": "string",
                    "enum": ["files_with_matches", "content", "count"],
                    "default": "files_with_matches"
                }
            },
            "required": ["pattern", "path"]
        })
    }

    fn max_timeout(&self) -> Duration {
        // ripgrep is fast, but a cold-cache traversal of a large
        // monorepo can still exceed the 30 s default. 60 s gives
        // headroom inside the per-mode MAX_HITS cap.
        Duration::from_secs(60)
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        let pattern = params.get("pattern").and_then(Value::as_str)?;
        let path = params.get("path").and_then(Value::as_str);
        crate::progress::preview_search(pattern, path)
    }

    fn accessed_resources(&self, params: &Value) -> Vec<ResourceAccess> {
        params
            .get("path")
            .and_then(|v| v.as_str())
            .map(|s| {
                vec![ResourceAccess::ReadFile {
                    path: PathBuf::from(s),
                }]
            })
            .unwrap_or_default()
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let mut p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        p.glob = p.glob.filter(|glob| !glob.trim().is_empty());

        require_absolute(&p.path, "Grep", "path")?;

        // Every hit in it would be filtered anyway; this keeps rg from
        // reading the file at all. Directories are left to the per-file
        // filter, whose verdict a directory's own name doesn't decide.
        let is_dir = tokio::fs::metadata(&p.path).await.is_ok_and(|m| m.is_dir());
        if !is_dir && baybo_security::is_sensitive_path(&p.path) {
            return Ok(ToolOutput::Text(NO_MATCHES_MESSAGE.to_string()));
        }

        run_rg(&self.process_manager, &p, ctx).await
    }
}

async fn run_rg(
    process_manager: &Arc<baybo_process::ProcessManager>,
    p: &Params,
    ctx: &ToolContext,
) -> crate::Result<ToolOutput> {
    let cap = rg::capture(process_manager, ctx, Hits::new(p.output_mode), |cmd| {
        cmd.arg("--hidden")
            .arg(format!("--max-filesize={MAX_FILE_BYTES}"));

        if p.case_insensitive {
            cmd.arg("--ignore-case");
        }
        if let Some(g) = &p.glob {
            cmd.arg("--glob").arg(g);
        }

        match p.output_mode {
            OutputMode::FilesWithMatches => {
                cmd.arg("--files-with-matches").arg("--null");
            }
            OutputMode::Count => {
                // rg omits the path when counting a single file operand,
                // and the parser (and the sensitive-path filter) needs it.
                cmd.arg("--count").arg("--with-filename").arg("--null");
            }
            OutputMode::Content => {
                // No file can contribute more rows than we keep, and the
                // cap stops rg from streaming a sensitive file's every hit
                // just for us to filter it.
                cmd.arg("--json")
                    .arg(format!("--max-count={}", MAX_HITS + 1));
            }
        }

        cmd.arg("--regexp").arg(&p.pattern).arg("--").arg(&p.path);
    })
    .await?;

    if cap.failed() {
        return Err(cap.into_error());
    }
    let hits = cap.stdout.finish(cap.stdout_closed_early).map_err(|e| {
        ToolError::Execution(format!(
            "unexpected rg output at byte {}: {}; no results reported",
            e.offset, e.reason
        ))
    })?;
    Ok(ToolOutput::Text(hits.render()))
}

/// Streams rg's stdout into result rows, applying the sensitive-path
/// filter record by record. Every cap counts only what survives the
/// filter, so how much a sensitive file matched never shows — not as a
/// row, a count, or a truncation notice.
#[derive(Debug)]
struct Hits {
    mode: OutputMode,
    pending: Vec<u8>,
    /// Stream offset of `pending[0]`, for format errors.
    offset: usize,
    /// Inside an over-long `--json` record, discarding up to its newline.
    skipping: bool,
    /// The file of the last `--json` `begin` event. rg prints each file's
    /// events as one unit, so this is the file a record belongs to.
    current_file: Option<CurrentFile>,
    rows: Vec<String>,
    row_bytes: usize,
    truncated: bool,
    omitted_non_utf8: usize,
    omitted_too_long: usize,
    error: Option<FormatError>,
}

#[derive(Debug)]
struct CurrentFile {
    path: Vec<u8>,
    sensitive: bool,
}

#[derive(Debug)]
struct FormatError {
    offset: usize,
    reason: &'static str,
}

enum Framed {
    /// A record of `len` bytes, delimiter included.
    Record(usize),
    Incomplete,
}

impl rg::StdoutSink for Hits {
    fn feed(&mut self, mut chunk: &[u8]) -> ControlFlow<()> {
        if self.skipping {
            let Some(newline) = chunk.iter().position(|&b| b == b'\n') else {
                self.offset += chunk.len();
                return ControlFlow::Continue(());
            };
            self.skipping = false;
            self.offset += newline + 1;
            chunk = &chunk[newline + 1..];
        }
        self.pending.extend_from_slice(chunk);

        let pending = std::mem::take(&mut self.pending);
        let mut start = 0;
        let flow = loop {
            let rest = &pending[start..];
            let Framed::Record(len) = self.frame(rest) else {
                break ControlFlow::Continue(());
            };
            let record = if len > MAX_RECORD_BYTES {
                self.oversized()
            } else {
                self.record(&rest[..len])
            };
            let flow = match record {
                Ok(flow) => flow,
                Err(reason) => self.fail(start, reason),
            };
            start += len;
            if flow.is_break() {
                break flow;
            }
        };
        self.offset += start;
        self.pending = pending;
        self.pending.drain(..start);

        if flow.is_continue() && self.pending.len() > MAX_RECORD_BYTES {
            if let Err(reason) = self.oversized() {
                return self.fail(0, reason);
            }
            self.offset += self.pending.len();
            self.pending.clear();
            self.skipping = true;
        }
        flow
    }
}

impl Hits {
    fn new(mode: OutputMode) -> Self {
        Self {
            mode,
            pending: Vec::new(),
            offset: 0,
            skipping: false,
            current_file: None,
            rows: Vec::new(),
            row_bytes: 0,
            truncated: false,
            omitted_non_utf8: 0,
            omitted_too_long: 0,
            error: None,
        }
    }

    fn fail(&mut self, at: usize, reason: &'static str) -> ControlFlow<()> {
        self.error = Some(FormatError {
            offset: self.offset + at,
            reason,
        });
        ControlFlow::Break(())
    }

    /// Find the end of the next record. Records are delimited by the one
    /// byte a Unix path can't hold — NUL — so a path with `:` or `\n`
    /// still frames correctly; `--count` adds `count\n` after it.
    fn frame(&self, buf: &[u8]) -> Framed {
        let position = |byte| buf.iter().position(|&b| b == byte);
        match self.mode {
            OutputMode::FilesWithMatches => match position(0) {
                Some(nul) => Framed::Record(nul + 1),
                None => Framed::Incomplete,
            },
            OutputMode::Count => {
                let Some(nul) = position(0) else {
                    return Framed::Incomplete;
                };
                match buf[nul..].iter().position(|&b| b == b'\n') {
                    Some(newline) => Framed::Record(nul + newline + 1),
                    None => Framed::Incomplete,
                }
            }
            OutputMode::Content => match position(b'\n') {
                Some(newline) => Framed::Record(newline + 1),
                None => Framed::Incomplete,
            },
        }
    }

    fn record(&mut self, record: &[u8]) -> Result<ControlFlow<()>, &'static str> {
        match self.mode {
            OutputMode::FilesWithMatches => {
                let path = absolute_path(&record[..record.len() - 1])?;
                Ok(self.admit(path, |path| Some(path.to_string())))
            }
            OutputMode::Count => {
                let (path, count) = parse_count_record(record)?;
                Ok(self.admit(path, |path| Some(format!("{path}:{count}"))))
            }
            OutputMode::Content => {
                let event: RgEvent =
                    serde_json::from_slice(record).map_err(|_| "record is not an rg JSON event")?;
                match event {
                    RgEvent::Begin { path } => {
                        let path = path.into_bytes()?;
                        let sensitive = is_sensitive(&path);
                        self.current_file = Some(CurrentFile { path, sensitive });
                        Ok(ControlFlow::Continue(()))
                    }
                    RgEvent::Match {
                        path,
                        lines,
                        line_number,
                    } => {
                        let path = path.into_bytes()?;
                        let line = lines.into_bytes()?;
                        Ok(self.admit(&path, |path| {
                            let line = std::str::from_utf8(&line).ok()?;
                            let line = line.trim_end_matches('\n');
                            Some(format!("{path}:{line_number}:{line}"))
                        }))
                    }
                    RgEvent::End(_) | RgEvent::Context(_) | RgEvent::Summary(_) => {
                        Ok(ControlFlow::Continue(()))
                    }
                }
            }
        }
    }

    /// Filter one hit and, if it may be shown, keep the row `render`
    /// builds from its path. `render` returns `None` when the hit has no
    /// UTF-8 text to show.
    fn admit(
        &mut self,
        path: &[u8],
        render: impl FnOnce(&str) -> Option<String>,
    ) -> ControlFlow<()> {
        let sensitive = match &self.current_file {
            Some(file) if file.path == path => file.sensitive,
            _ => is_sensitive(path),
        };
        if sensitive {
            return ControlFlow::Continue(());
        }
        let path = Path::new(OsStr::from_bytes(path));
        let Some(row) = path.to_str().and_then(|path| render(&display_path(path))) else {
            self.omitted_non_utf8 += 1;
            return ControlFlow::Continue(());
        };
        if self.rows.len() >= MAX_HITS || self.row_bytes + row.len() > MAX_OUTPUT_BYTES {
            self.truncated = true;
            return ControlFlow::Break(());
        }
        self.row_bytes += row.len();
        self.rows.push(row);
        ControlFlow::Continue(())
    }

    /// A record outgrew [`MAX_RECORD_BYTES`]. Only a `--json` match line
    /// can (paths are bounded far below it): it is dropped, and reported
    /// as omitted unless its file is sensitive.
    fn oversized(&mut self) -> Result<ControlFlow<()>, &'static str> {
        if !matches!(self.mode, OutputMode::Content) {
            return Err("record exceeds the size limit");
        }
        let file = self
            .current_file
            .as_ref()
            .ok_or("match record precedes its file's begin event")?;
        if !file.sensitive {
            self.omitted_too_long += 1;
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Close out the stream. Unless we stopped reading ourselves (a cap
    /// was hit, or a format error), rg must have ended on a record
    /// boundary.
    fn finish(self, closed_early: bool) -> Result<Self, FormatError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if !closed_early && (self.skipping || !self.pending.is_empty()) {
            return Err(FormatError {
                offset: self.offset,
                reason: "output ends mid-record",
            });
        }
        Ok(self)
    }

    fn render(mut self) -> String {
        if self.rows.is_empty()
            && !self.truncated
            && self.omitted_non_utf8 == 0
            && self.omitted_too_long == 0
        {
            return NO_MATCHES_MESSAGE.to_string();
        }
        if self.truncated {
            self.rows.push(format!(
                "… [truncated to {MAX_HITS} results or {MAX_OUTPUT_MIB} MiB — narrow `path` or `glob`]"
            ));
        }
        if self.omitted_non_utf8 > 0 {
            self.rows.push(format!(
                "… [{} result(s) omitted: not valid UTF-8]",
                self.omitted_non_utf8
            ));
        }
        if self.omitted_too_long > 0 {
            self.rows.push(format!(
                "… [{} matching line(s) omitted: too large to report]",
                self.omitted_too_long
            ));
        }
        self.rows.join("\n")
    }
}

fn is_sensitive(path: &[u8]) -> bool {
    baybo_security::is_sensitive_path(Path::new(OsStr::from_bytes(path)))
}

/// Every path rg prints descends from the absolute operand; anything else
/// means the stream isn't framed the way we asked for (e.g. rg's
/// path-less `count\n` form glued onto the next record).
fn absolute_path(path: &[u8]) -> Result<&[u8], &'static str> {
    if path.first() == Some(&b'/') {
        Ok(path)
    } else {
        Err("path is not absolute")
    }
}

/// Split a framed `path\0count\n` record.
fn parse_count_record(record: &[u8]) -> Result<(&[u8], u64), &'static str> {
    let nul = record
        .iter()
        .position(|&b| b == 0)
        .ok_or("record has no NUL path terminator")?;
    let path = absolute_path(&record[..nul])?;
    let digits = &record[nul + 1..record.len() - 1];
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err("count is not a decimal number");
    }
    let count = std::str::from_utf8(digits)
        .ok()
        .and_then(|d| d.parse::<u64>().ok())
        .ok_or("count is out of range")?;
    Ok((path, count))
}

/// A path that would break the one-row-per-result layout (a `\n` in a
/// Unix filename) is rendered quoted and escaped instead.
fn display_path(path: &str) -> Cow<'_, str> {
    if path.chars().any(char::is_control) {
        Cow::Owned(format!("{path:?}"))
    } else {
        Cow::Borrowed(path)
    }
}

/// rg's `--json` event stream. `end`, `context` and `summary` carry
/// nothing we report; an event type outside this set is a format error.
#[derive(Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum RgEvent {
    Begin {
        path: RgData,
    },
    Match {
        path: RgData,
        lines: RgData,
        line_number: u64,
    },
    End(IgnoredAny),
    Context(IgnoredAny),
    Summary(IgnoredAny),
}

/// rg's encoding of a path or line: `text` when it is UTF-8, otherwise
/// base64 `bytes`.
#[derive(Deserialize)]
#[serde(untagged)]
enum RgData {
    Text { text: String },
    Bytes { bytes: String },
}

impl RgData {
    fn into_bytes(self) -> Result<Vec<u8>, &'static str> {
        match self {
            Self::Text { text } => Ok(text.into_bytes()),
            Self::Bytes { bytes } => base64::engine::general_purpose::STANDARD
                .decode(bytes)
                .map_err(|_| "rg JSON bytes field is not base64"),
        }
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

    #[tokio::test]
    async fn files_with_matches_mode() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.rs"), "needle here")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("b.rs"), "nothing")
            .await
            .unwrap();

        let out = GrepTool::for_test()
            .execute(json!({ "pattern": "needle", "path": dir.path() }), &ctx())
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else {
            panic!();
        };
        assert!(s.contains("a.rs"));
        assert!(!s.contains("b.rs"));
    }

    #[tokio::test]
    async fn rejects_relative_path() {
        let err = GrepTool::for_test()
            .execute(json!({ "pattern": "x", "path": "relative/dir" }), &ctx())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParams(ref m) if m.contains("absolute")),
            "expected InvalidParams about absolute, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn rejects_missing_path() {
        let err = GrepTool::for_test()
            .execute(json!({ "pattern": "x" }), &ctx())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParams(_)),
            "expected InvalidParams, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn skips_sensitive_subtree() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        tokio::fs::create_dir(&ssh).await.unwrap();
        // Plant a "matching" line inside what looks like a private key.
        tokio::fs::write(ssh.join("id_rsa"), "AKIA-needle-leak")
            .await
            .unwrap();
        // And a regular file with the same needle so we know the search ran.
        tokio::fs::write(dir.path().join("ok.txt"), "AKIA-needle-leak")
            .await
            .unwrap();

        let out = GrepTool::for_test()
            .execute(
                json!({
                    "pattern": "needle",
                    "path": dir.path(),
                    "output_mode": "content"
                }),
                &ctx(),
            )
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else {
            panic!();
        };
        assert!(s.contains("ok.txt"), "regular file should match: {s}");
        assert!(!s.contains("id_rsa"), "sensitive file leaked: {s}");
        assert!(!s.contains(".ssh"), "sensitive dir leaked: {s}");
    }

    #[tokio::test]
    async fn content_mode_with_glob_filter() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.rs"), "needle")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "needle")
            .await
            .unwrap();

        let out = GrepTool::for_test()
            .execute(
                json!({
                    "pattern": "needle",
                    "path": dir.path(),
                    "glob": "*.rs",
                    "output_mode": "content"
                }),
                &ctx(),
            )
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else {
            panic!();
        };
        assert!(s.contains("a.rs"));
        assert!(!s.contains("a.txt"));
    }

    #[tokio::test]
    async fn an_empty_glob_filler_does_not_filter_every_file() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "needle")
            .await
            .unwrap();

        let out = GrepTool::for_test()
            .execute(
                json!({
                    "pattern": "needle",
                    "path": dir.path(),
                    "glob": "",
                    "output_mode": "content"
                }),
                &ctx(),
            )
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else { panic!() };
        assert!(s.contains("a.txt"), "{s}");
    }

    #[test]
    fn the_optional_output_mode_leads_with_its_default() {
        let schema = GrepTool::for_test().parameters_schema();
        assert_eq!(
            schema["properties"]["output_mode"]["enum"][0],
            "files_with_matches"
        );
        assert_eq!(
            schema["properties"]["output_mode"]["default"],
            "files_with_matches"
        );
    }

    /// Regression for the colon-in-path bypass that codex flagged: a
    /// sensitive `.env` under a directory whose own name contains `:`
    /// must not leak. Pre-fix, `content` mode parsed `path:line:match`
    /// with `split_once(':')`, classified the path as the unrelated
    /// prefix `<tmp>/work`, and returned the secret.
    #[tokio::test]
    async fn content_mode_handles_colon_in_path() {
        let dir = tempfile::tempdir().unwrap();
        let weird = dir.path().join("work:copy");
        if tokio::fs::create_dir(&weird).await.is_err() {
            // Some filesystems (HFS+) reject `:`; nothing to test there.
            return;
        }
        tokio::fs::write(weird.join(".env"), "SECRET=needle-value")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("ok.txt"), "needle")
            .await
            .unwrap();

        let out = GrepTool::for_test()
            .execute(
                json!({
                    "pattern": "needle",
                    "path": dir.path(),
                    "output_mode": "content"
                }),
                &ctx(),
            )
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else {
            panic!();
        };
        assert!(s.contains("ok.txt"), "regular file should match: {s}");
        assert!(
            !s.contains(".env"),
            ".env path under colon-containing dir leaked: {s}"
        );
        assert!(
            !s.contains("SECRET=needle-value"),
            "sensitive file content leaked: {s}"
        );
    }

    async fn grep_text(params: Value) -> String {
        let out = GrepTool::for_test().execute(params, &ctx()).await.unwrap();
        let ToolOutput::Text(s) = out else {
            panic!("expected text output, got: {out:?}");
        };
        s
    }

    async fn count(path: &Path, extra: Value) -> String {
        let mut params = json!({ "pattern": "needle", "path": path, "output_mode": "count" });
        if let (Value::Object(p), Value::Object(e)) = (&mut params, extra) {
            p.extend(e);
        }
        grep_text(params).await
    }

    /// The reported bug: `rg --count` on a single file operand prints a
    /// bare `2\n`, which the parser dropped, so the call came back blank.
    #[tokio::test]
    async fn count_on_a_single_file_reports_its_path_and_count() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("synthetic.txt");
        tokio::fs::write(&file, "needle\nhay\nneedle needle\n")
            .await
            .unwrap();

        let s = count(&file, json!({})).await;
        assert_eq!(s, format!("{}:2", file.display()));
    }

    #[tokio::test]
    async fn count_on_a_single_file_without_a_match_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hay.txt");
        tokio::fs::write(&file, "hay\n").await.unwrap();
        let empty = dir.path().join("empty.txt");
        tokio::fs::write(&empty, "").await.unwrap();

        assert_eq!(count(&file, json!({})).await, NO_MATCHES_MESSAGE);
        assert_eq!(count(&empty, json!({})).await, NO_MATCHES_MESSAGE);
    }

    #[tokio::test]
    async fn count_over_a_directory_lists_only_matching_files() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "needle\nneedle\nneedle\n")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("b.txt"), "needle\n")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("c.txt"), "hay\n")
            .await
            .unwrap();

        let s = count(dir.path(), json!({})).await;
        let mut rows: Vec<&str> = s.lines().collect();
        rows.sort_unstable();
        assert_eq!(
            rows,
            [
                format!("{}:3", dir.path().join("a.txt").display()),
                format!("{}:1", dir.path().join("b.txt").display()),
            ]
        );

        let none = tempfile::tempdir().unwrap();
        tokio::fs::write(none.path().join("c.txt"), "hay\n")
            .await
            .unwrap();
        assert_eq!(count(none.path(), json!({})).await, NO_MATCHES_MESSAGE);
    }

    #[tokio::test]
    async fn count_honours_the_glob_and_ignores_a_blank_one() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.rs"), "needle\n")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "needle\n")
            .await
            .unwrap();

        for blank in [json!({}), json!({ "glob": "" }), json!({ "glob": "   " })] {
            assert_eq!(count(dir.path(), blank).await.lines().count(), 2);
        }
        let s = count(dir.path(), json!({ "glob": "*.rs" })).await;
        assert_eq!(s, format!("{}:1", dir.path().join("a.rs").display()));
        let s = count(dir.path(), json!({ "glob": "*.md" })).await;
        assert_eq!(s, NO_MATCHES_MESSAGE);
    }

    #[tokio::test]
    async fn count_keeps_odd_paths_on_one_row() {
        let dir = tempfile::tempdir().unwrap();
        let spaced = dir.path().join("a b:c.txt");
        tokio::fs::write(&spaced, "needle\n").await.unwrap();
        let newline = dir.path().join("new\nline.txt");
        tokio::fs::write(&newline, "needle\nneedle\n")
            .await
            .unwrap();

        let s = count(dir.path(), json!({})).await;
        let mut rows: Vec<&str> = s.lines().collect();
        rows.sort_unstable();
        let newline_row = format!("{:?}:2", newline.to_str().unwrap());
        let spaced_row = format!("{}:1", spaced.display());
        let mut want = [newline_row.as_str(), spaced_row.as_str()];
        want.sort_unstable();
        assert_eq!(rows, want);
    }

    #[tokio::test]
    async fn count_reports_non_utf8_paths_as_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let name = OsStr::from_bytes(b"bad-\xff.txt");
        tokio::fs::write(dir.path().join(name), "needle\n")
            .await
            .unwrap();

        let s = count(dir.path(), json!({})).await;
        assert_eq!(s, "… [1 result(s) omitted: not valid UTF-8]");
    }

    /// A sensitive file's hits must read exactly like no hits at all —
    /// anything else is a per-pattern oracle on the file's contents.
    #[tokio::test]
    async fn count_filters_sensitive_paths_indistinguishably_from_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        tokio::fs::create_dir(&ssh).await.unwrap();
        let key = ssh.join("id_rsa");
        tokio::fs::write(&key, "needle-placeholder\n")
            .await
            .unwrap();

        assert_eq!(count(&key, json!({})).await, NO_MATCHES_MESSAGE);
        assert_eq!(count(dir.path(), json!({})).await, NO_MATCHES_MESSAGE);

        tokio::fs::write(dir.path().join("ok.txt"), "needle\n")
            .await
            .unwrap();
        let s = count(dir.path(), json!({})).await;
        assert_eq!(s, format!("{}:1", dir.path().join("ok.txt").display()));
    }

    #[tokio::test]
    async fn path_modes_say_when_nothing_matched() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "hay\n")
            .await
            .unwrap();
        for mode in ["files_with_matches", "content"] {
            let s = grep_text(json!({
                "pattern": "needle",
                "path": dir.path(),
                "output_mode": mode
            }))
            .await;
            assert_eq!(s, NO_MATCHES_MESSAGE, "{mode}");
        }
    }

    #[tokio::test]
    async fn count_surfaces_an_rg_error_instead_of_a_blank() {
        let dir = tempfile::tempdir().unwrap();
        let err = GrepTool::for_test()
            .execute(
                json!({
                    "pattern": "needle",
                    "path": dir.path().join("missing.txt"),
                    "output_mode": "count"
                }),
                &ctx(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Execution(ref m) if m.contains("exited with code 2")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn path_and_content_modes_report_non_utf8_hits_as_omitted() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("latin1.txt"), b"caf\xe9 needle\n")
            .await
            .unwrap();
        let s =
            grep_text(json!({ "pattern": "needle", "path": dir.path(), "output_mode": "content" }))
                .await;
        assert_eq!(s, "… [1 result(s) omitted: not valid UTF-8]");

        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(
            dir.path().join(OsStr::from_bytes(b"bad-\xff.txt")),
            "needle\n",
        )
        .await
        .unwrap();
        let s = grep_text(json!({ "pattern": "needle", "path": dir.path() })).await;
        assert_eq!(s, "… [1 result(s) omitted: not valid UTF-8]");
    }

    /// rg prints far more than any in-process cap for this file; that
    /// volume must not surface as a truncation notice once its rows are
    /// filtered, or the notice becomes a per-pattern oracle on it.
    #[tokio::test]
    async fn a_huge_sensitive_match_set_reads_exactly_like_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let history = "needle in a synthetic history line\n".repeat(150_000);
        tokio::fs::write(dir.path().join(".zsh_history"), history)
            .await
            .unwrap();
        for mode in ["content", "count", "files_with_matches"] {
            for pattern in ["needle", "absent-pattern"] {
                let s = grep_text(
                    json!({ "pattern": pattern, "path": dir.path(), "output_mode": mode }),
                )
                .await;
                assert_eq!(s, NO_MATCHES_MESSAGE, "{mode} {pattern}");
            }
        }

        tokio::fs::write(dir.path().join("ok.txt"), "needle\n")
            .await
            .unwrap();
        let s =
            grep_text(json!({ "pattern": "needle", "path": dir.path(), "output_mode": "content" }))
                .await;
        assert_eq!(
            s,
            format!("{}:1:needle", dir.path().join("ok.txt").display())
        );
    }

    #[tokio::test]
    async fn an_oversized_matching_line_is_reported_as_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let line = format!("needle{}\nneedle short\n", "x".repeat(MAX_RECORD_BYTES));
        tokio::fs::write(dir.path().join("wide.txt"), line)
            .await
            .unwrap();
        let s =
            grep_text(json!({ "pattern": "needle", "path": dir.path(), "output_mode": "content" }))
                .await;
        assert_eq!(
            s,
            format!(
                "{}:2:needle short\n… [1 matching line(s) omitted: too large to report]",
                dir.path().join("wide.txt").display()
            )
        );
    }

    #[tokio::test]
    async fn a_directory_named_like_a_secret_file_is_still_searched() {
        let dir = tempfile::tempdir().unwrap();
        let conf = dir.path().join(".env.d");
        tokio::fs::create_dir(&conf).await.unwrap();
        tokio::fs::write(conf.join("app.conf"), "needle\n")
            .await
            .unwrap();
        let s = grep_text(json!({ "pattern": "needle", "path": conf })).await;
        assert_eq!(s, conf.join("app.conf").display().to_string());
    }

    /// rg's JSON record repeats each submatch, so a modest line with many
    /// hits makes a record far larger than the line; it must still show.
    #[tokio::test]
    async fn a_line_with_many_submatches_is_still_reported() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("dense.txt");
        let line = "a".repeat(30_000);
        tokio::fs::write(&file, format!("{line}\n")).await.unwrap();
        let s = grep_text(json!({ "pattern": "a", "path": file, "output_mode": "content" })).await;
        assert_eq!(s, format!("{}:1:{line}", file.display()));
    }

    /// Run `stdout` through a [`Hits`] sink both in one chunk and one
    /// byte at a time; framing must not depend on where reads split.
    fn sink(mode: OutputMode, stdout: &[u8]) -> Result<String, &'static str> {
        use rg::StdoutSink;
        let run = |chunks: &mut dyn Iterator<Item = &[u8]>| {
            let mut hits = Hits::new(mode);
            let mut closed_early = false;
            for chunk in chunks {
                if hits.feed(chunk).is_break() {
                    closed_early = true;
                    break;
                }
            }
            hits.finish(closed_early)
                .map(Hits::render)
                .map_err(|e| e.reason)
        };
        let whole = run(&mut std::iter::once(stdout));
        let bytewise = run(&mut stdout.chunks(1));
        assert_eq!(whole, bytewise, "chunking changed the result");
        whole
    }

    #[test]
    fn count_records_split_on_nul_not_on_newline() {
        assert_eq!(
            sink(OutputMode::Count, b"/a:b\x003\n/new\nline\x0012\n"),
            Ok("/a:b:3\n\"/new\\nline\":12".to_string())
        );
        assert_eq!(
            sink(OutputMode::Count, b""),
            Ok(NO_MATCHES_MESSAGE.to_string())
        );
    }

    #[test]
    fn malformed_count_output_is_an_error_not_a_blank() {
        let err = |stdout: &[u8]| sink(OutputMode::Count, stdout).unwrap_err();
        assert_eq!(err(b"2\n"), "output ends mid-record");
        assert_eq!(err(b"5\n/a\x003\n"), "path is not absolute");
        assert_eq!(err(b"\x003\n"), "path is not absolute");
        assert_eq!(err(b"/a\x00\n"), "count is not a decimal number");
        assert_eq!(err(b"/a\x00-1\n"), "count is not a decimal number");
        assert_eq!(err(b"/a\x003 \n"), "count is not a decimal number");
        assert_eq!(
            err(b"/a\x0099999999999999999999\n"),
            "count is out of range"
        );
        assert_eq!(err(b"/a\x003"), "output ends mid-record");
        assert_eq!(err(b"/a\x003\n/b"), "output ends mid-record");

        let mut hits = Hits::new(OutputMode::Count);
        let _ = rg::StdoutSink::feed(&mut hits, b"/a\x001\n/b\x00x\n");
        assert_eq!(hits.finish(false).unwrap_err().offset, 5);
    }

    #[test]
    fn malformed_path_and_json_output_is_an_error() {
        assert_eq!(
            sink(OutputMode::FilesWithMatches, b"/a\x00rel\x00"),
            Err("path is not absolute")
        );
        assert_eq!(
            sink(OutputMode::FilesWithMatches, b"/a"),
            Err("output ends mid-record")
        );
        assert_eq!(
            sink(OutputMode::Content, b"not json\n"),
            Err("record is not an rg JSON event")
        );
    }

    #[test]
    fn json_events_decode_bytes_and_skip_the_rest() {
        let begin = br#"{"type":"begin","data":{"path":{"text":"/a"}}}"#;
        let hit = br#"{"type":"match","data":{"path":{"text":"/a"},"lines":{"text":"x needle\n"},"line_number":7,"absolute_offset":0,"submatches":[]}}"#;
        let bad = br#"{"type":"match","data":{"path":{"text":"/a"},"lines":{"bytes":"/w=="},"line_number":8,"absolute_offset":0,"submatches":[]}}"#;
        let end =
            br#"{"type":"end","data":{"path":{"text":"/a"},"binary_offset":null,"stats":{}}}"#;
        let summary = br#"{"data":{"elapsed_total":{}},"type":"summary"}"#;
        let stdout = [&begin[..], hit, bad, end, summary].join(&b'\n');
        let stdout = [stdout.as_slice(), b"\n"].concat();
        assert_eq!(
            sink(OutputMode::Content, &stdout),
            Ok("/a:7:x needle\n… [1 result(s) omitted: not valid UTF-8]".to_string())
        );
    }

    #[test]
    fn an_oversized_json_line_is_omitted_unless_its_file_is_sensitive() {
        let long = "x".repeat(MAX_RECORD_BYTES + 1);
        let stdout = |path: &str| {
            format!(
                "{{\"type\":\"begin\",\"data\":{{\"path\":{{\"text\":\"{path}\"}}}}}}\n\
                 {{\"type\":\"match\",\"data\":{{\"path\":{{\"text\":\"{path}\"}},\"lines\":{{\"text\":\"{long}\"}},\"line_number\":1}}}}\n"
            )
        };
        let mut hits = Hits::new(OutputMode::Content);
        let _ = rg::StdoutSink::feed(&mut hits, stdout("/a/b.txt").as_bytes());
        assert_eq!(
            hits.finish(false).unwrap().render(),
            "… [1 matching line(s) omitted: too large to report]"
        );

        let mut hits = Hits::new(OutputMode::Content);
        let _ = rg::StdoutSink::feed(&mut hits, stdout("/a/.ssh/config").as_bytes());
        assert_eq!(hits.finish(false).unwrap().render(), NO_MATCHES_MESSAGE);
    }

    #[test]
    fn caps_count_only_rows_that_survive_the_filter() {
        let mut stdout = Vec::new();
        for i in 0..MAX_HITS * 2 {
            stdout.extend_from_slice(format!("/x/.ssh/k{i}\x001\n").as_bytes());
        }
        stdout.extend_from_slice(b"/x/ok\x001\n");
        assert_eq!(sink(OutputMode::Count, &stdout), Ok("/x/ok:1".to_string()));

        let mut stdout = Vec::new();
        for i in 0..=MAX_HITS {
            stdout.extend_from_slice(format!("/x/f{i}\x00").as_bytes());
        }
        let out = sink(OutputMode::FilesWithMatches, &stdout).unwrap();
        assert_eq!(out.lines().count(), MAX_HITS + 1);
        assert!(out.ends_with("narrow `path` or `glob`]"), "{out}");
    }
}
