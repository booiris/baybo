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

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::paths::require_absolute;
use super::rg;
use crate::{ResourceAccess, Tool, ToolConcurrency, ToolContext, ToolError, ToolOutput};

const MAX_HITS: usize = 500;
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_FILE_MIB: u64 = MAX_FILE_BYTES / 1024 / 1024;

static DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Search file contents with a regular expression by spawning the \
         `rg` (ripgrep) binary. `output_mode` may be `content` (matching lines), \
         `files_with_matches` (default, paths only), or `count` (match \
         counts per file). Supports file-type filtering via the `glob` \
         parameter. Sensitive paths (SSH/AWS/GPG configs, .env, \
         /etc/shadow, …) are filtered out of the output so their contents \
         never enter the result.\n\n\
         BEFORE SEARCHING: For an unfamiliar directory, first probe its \
         scale with `Glob` (e.g. count entries) and narrow the search root \
         or `glob` filter accordingly. Files larger than {MAX_FILE_MIB} MiB \
         are skipped, and per-mode results are capped at {MAX_HITS}."
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
    #[serde(default = "default_output_mode")]
    output_mode: String,
}

fn default_output_mode() -> String {
    "files_with_matches".into()
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
                "path":             { "type": "string", "description": "Absolute directory to search (required)" },
                "glob":             { "type": "string", "description": "Filename glob to filter files (e.g. `*.rs`). An empty string means no filename filter." },
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

        match p.output_mode.as_str() {
            "content" | "files_with_matches" | "count" => {}
            other => {
                return Err(ToolError::InvalidParams(format!(
                    "unknown output_mode `{other}`"
                )));
            }
        }

        run_rg(&self.process_manager, &p, ctx).await
    }
}

async fn run_rg(
    process_manager: &Arc<baybo_process::ProcessManager>,
    p: &Params,
    ctx: &ToolContext,
) -> crate::Result<ToolOutput> {
    let cap = rg::capture(process_manager, ctx, rg::MAX_STDOUT_BYTES, |cmd| {
        cmd.arg("--hidden")
            .arg(format!("--max-filesize={MAX_FILE_BYTES}"));

        if p.case_insensitive {
            cmd.arg("--ignore-case");
        }
        if let Some(g) = &p.glob {
            cmd.arg("--glob").arg(g);
        }

        match p.output_mode.as_str() {
            "files_with_matches" => {
                cmd.arg("--files-with-matches").arg("--null");
            }
            "count" => {
                cmd.arg("--count").arg("--null");
            }
            "content" => {
                cmd.arg("--json");
            }
            _ => unreachable!(),
        }

        cmd.arg("--regexp").arg(&p.pattern).arg("--").arg(&p.path);
    })
    .await?;

    // rg exit codes: 0 = matches, 1 = no matches, 2+ = real error.
    if cap.code >= 2 {
        return Err(cap.into_error());
    }
    let stdout = cap.stdout;

    let mut kept: Vec<String> = Vec::new();
    let mut hits_truncated = false;

    let push = |row: String, kept: &mut Vec<String>, truncated: &mut bool| {
        if kept.len() >= MAX_HITS {
            *truncated = true;
            return false;
        }
        kept.push(row);
        true
    };

    match p.output_mode.as_str() {
        "files_with_matches" => {
            for path in rg::iter_null_paths(&stdout) {
                if baybo_security::is_sensitive_path(Path::new(&path)) {
                    continue;
                }
                if !push(path, &mut kept, &mut hits_truncated) {
                    break;
                }
            }
        }
        "count" => {
            for (path, count) in iter_count_records(&stdout) {
                if baybo_security::is_sensitive_path(Path::new(&path)) {
                    continue;
                }
                if !push(format!("{path}:{count}"), &mut kept, &mut hits_truncated) {
                    break;
                }
            }
        }
        "content" => {
            for hit in iter_json_matches(&stdout) {
                if baybo_security::is_sensitive_path(Path::new(&hit.path)) {
                    continue;
                }
                let row = format!("{}:{}:{}", hit.path, hit.line_number, hit.line_text);
                if !push(row, &mut kept, &mut hits_truncated) {
                    break;
                }
            }
        }
        _ => unreachable!(),
    }

    let mut body = kept.join("\n");
    if hits_truncated {
        body.push_str(&format!("\n… [truncated to {MAX_HITS} results]"));
    }
    Ok(ToolOutput::Text(body))
}

/// Decode `path\0count\n` records from `rg --count --null`. Uses `\n`
/// to delimit records and `\0` to split each record into path + count
/// — the path itself may contain `:` or other punctuation, so the NUL
/// boundary is the only unambiguous split.
fn iter_count_records(stdout: &[u8]) -> impl Iterator<Item = (String, String)> + '_ {
    stdout
        .split(|b| *b == b'\n')
        .filter(|s| !s.is_empty())
        .filter_map(|line| {
            let nul = line.iter().position(|&b| b == 0)?;
            let path = std::str::from_utf8(&line[..nul]).ok()?;
            let count = std::str::from_utf8(&line[nul + 1..]).ok()?;
            Some((path.to_string(), count.to_string()))
        })
}

struct ContentHit {
    path: String,
    line_number: u64,
    line_text: String,
}

/// Walk `rg --json` NDJSON and yield `match` events. Non-`match`
/// events (`begin`/`end`/`summary`/`context`) are skipped. Matches
/// whose path or line text is non-UTF-8 (so rg sends `bytes` instead
/// of `text`) are dropped — we don't have a UTF-8 string to feed the
/// caller's text result without lossy conversion that could hide a
/// sensitive substring.
fn iter_json_matches(stdout: &[u8]) -> impl Iterator<Item = ContentHit> + '_ {
    #[derive(Deserialize)]
    struct RgEvent {
        #[serde(rename = "type")]
        kind: String,
        data: Value,
    }
    stdout
        .split(|b| *b == b'\n')
        .filter(|s| !s.is_empty())
        .filter_map(|line| {
            let evt: RgEvent = serde_json::from_slice(line).ok()?;
            if evt.kind != "match" {
                return None;
            }
            let path = evt
                .data
                .get("path")
                .and_then(|p| p.get("text"))
                .and_then(|t| t.as_str())?
                .to_string();
            let raw_line = evt
                .data
                .get("lines")
                .and_then(|l| l.get("text"))
                .and_then(|t| t.as_str())?;
            let line_text = raw_line.trim_end_matches('\n').to_string();
            let line_number = evt.data.get("line_number").and_then(|n| n.as_u64())?;
            Some(ContentHit {
                path,
                line_number,
                line_text,
            })
        })
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
}
