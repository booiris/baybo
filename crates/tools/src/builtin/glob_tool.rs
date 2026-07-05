//! `Glob` — filename matching via the `rg` (ripgrep) binary, results
//! sorted by modification time (newest first).
//!
//! Backed by `rg --files --glob <pattern>` so pattern semantics match
//! `Grep` and ripgrep: a pattern with no `/` matches a file's basename
//! at any depth (`*.rs` finds every Rust file in the tree), while `/`
//! anchors the match relative to the search root.

use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::paths::require_absolute;
use super::rg;
use crate::{ResourceAccess, Tool, ToolConcurrency, ToolContext, ToolError, ToolOutput};

const MAX_RESULTS: usize = 1000;
const NO_MATCHES_MESSAGE: &str = "No files found";

static DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Find files by glob pattern (e.g. `**/*.rs`), backed by ripgrep. \
         Always use this instead of Bash commands like find or ls for \
         file searching. Results are absolute paths sorted by \
         modification time, newest first, and capped at {MAX_RESULTS} \
         entries.\n\n\
         PATHS: `path` is REQUIRED and MUST be an absolute filesystem \
         path. Relative paths and omission are rejected. `pattern` MUST \
         be relative to `path` — absolute patterns (e.g. `/etc/**/*`) \
         are rejected.\n\n\
         PATTERN SEMANTICS: a pattern with no `/` matches a file's name \
         at ANY depth — `*.rs` finds every Rust file under `path`, and \
         `*` finds every file. `*` never crosses `/`, so add separators \
         to anchor depth: `src/*.rs` matches `.rs` directly under \
         `src/`, `**/*.rs` matches at any depth. To narrow a broad \
         search, tighten `path` rather than widening the pattern."
    )
});

pub struct GlobTool;

#[derive(Debug, Deserialize)]
struct Params {
    pattern: String,
    path: PathBuf,
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    /// Read-only filesystem walk — safe to run concurrently.
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
                "pattern": { "type": "string", "description": "Glob pattern to match, relative to `path`" },
                "path":    { "type": "string", "description": "Absolute directory to search in (required)" }
            },
            "required": ["pattern", "path"]
        })
    }

    fn max_timeout(&self) -> Duration {
        // `--sortr=modified` forces a single-threaded traversal, so a
        // cold-cache walk of a large monorepo can exceed the 30 s
        // default; 60 s gives headroom.
        Duration::from_secs(60)
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        let pattern = params.get("pattern").and_then(Value::as_str)?;
        let path = params.get("path").and_then(Value::as_str);
        crate::progress::preview_search(pattern, path)
    }

    fn accessed_resources(&self, params: &Value) -> Vec<ResourceAccess> {
        // Glob enumerates filenames within a directory; treat the search root
        // as a read access so directory approvals cover subsequent reads.
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
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        require_absolute(&p.path, "Glob", "path")?;

        if std::path::Path::new(&p.pattern).is_absolute() {
            return Err(ToolError::InvalidParams(format!(
                "Glob `pattern` must be relative to `path`, got absolute `{}`",
                p.pattern
            )));
        }

        run_glob(&p, ctx).await
    }
}

async fn run_glob(p: &Params, ctx: &ToolContext) -> crate::Result<ToolOutput> {
    // `--files` lists files only (never directories); `--glob` filters by
    // pattern; `--sortr=modified` streams newest-first so truncating the
    // tail keeps the most recent matches; `--hidden` + `--no-ignore`
    // mirror a plain filesystem walk (dotfiles included, .gitignore not
    // applied); `--null` delimits paths unambiguously.
    let cap = rg::capture(ctx, rg::MAX_STDOUT_BYTES, |cmd| {
        cmd.arg("--files")
            .arg("--hidden")
            .arg("--no-ignore")
            .arg("--sortr=modified")
            .arg("--null")
            .arg("--glob")
            .arg(&p.pattern)
            .arg("--")
            .arg(&p.path);
    })
    .await?;

    // rg exits 2 both on a bad glob pattern (empty stdout) and on an
    // unreadable entry hit mid-traversal (partial stdout). Surface the
    // former; keep the partial listing for the latter, matching the old
    // filesystem walk that silently skipped unreadable entries.
    if cap.code >= 2 && cap.stdout.is_empty() {
        return Err(cap.into_error());
    }

    let mut paths: Vec<String> = rg::iter_null_paths(&cap.stdout).collect();
    if paths.is_empty() {
        return Ok(ToolOutput::Text(NO_MATCHES_MESSAGE.to_string()));
    }

    let truncated = paths.len() > MAX_RESULTS;
    paths.truncate(MAX_RESULTS);
    if truncated {
        paths.push(format!(
            "… [truncated to {MAX_RESULTS} results — narrow the `path` or use a more specific pattern]"
        ));
    }
    Ok(ToolOutput::Text(paths.join("\n")))
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
    async fn rejects_relative_path() {
        let err = GlobTool
            .execute(json!({ "pattern": "*.rs", "path": "relative/dir" }), &ctx())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParams(ref m) if m.contains("absolute")),
            "expected InvalidParams about absolute, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn rejects_missing_path() {
        let err = GlobTool
            .execute(json!({ "pattern": "*.rs" }), &ctx())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParams(_)),
            "expected InvalidParams, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn rejects_absolute_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let err = GlobTool
            .execute(
                json!({ "pattern": "/etc/**/*", "path": dir.path() }),
                &ctx(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParams(ref m) if m.contains("relative")),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn finds_matching_files() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.rs"), "").await.unwrap();
        tokio::fs::write(dir.path().join("b.rs"), "").await.unwrap();
        tokio::fs::write(dir.path().join("c.txt"), "")
            .await
            .unwrap();

        let out = GlobTool
            .execute(json!({ "pattern": "*.rs", "path": dir.path() }), &ctx())
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else {
            panic!();
        };
        assert!(s.contains("a.rs"));
        assert!(s.contains("b.rs"));
        assert!(!s.contains("c.txt"));
    }

    /// A bare `*` matches file basenames at any depth (ripgrep semantics).
    /// Regression: the old `glob` crate treated `*` as non-recursive, so
    /// a directory whose top level held only subdirectories returned
    /// nothing — the empty result that motivated the rg backend.
    #[tokio::test]
    async fn star_matches_files_at_any_depth() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        tokio::fs::create_dir(&sub).await.unwrap();
        tokio::fs::write(sub.join("deep.rs"), "").await.unwrap();

        let out = GlobTool
            .execute(json!({ "pattern": "*", "path": dir.path() }), &ctx())
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else {
            panic!();
        };
        assert!(s.contains("deep.rs"), "expected recursive match, got: {s}");
        assert_ne!(s, NO_MATCHES_MESSAGE);
    }

    #[tokio::test]
    async fn extension_pattern_matches_nested_files() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("a").join("b");
        tokio::fs::create_dir_all(&sub).await.unwrap();
        tokio::fs::write(dir.path().join("top.rs"), "")
            .await
            .unwrap();
        tokio::fs::write(sub.join("low.rs"), "").await.unwrap();
        tokio::fs::write(sub.join("note.txt"), "").await.unwrap();

        let out = GlobTool
            .execute(json!({ "pattern": "*.rs", "path": dir.path() }), &ctx())
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else {
            panic!();
        };
        assert!(s.contains("top.rs"));
        assert!(s.contains("low.rs"));
        assert!(!s.contains("note.txt"));
    }

    /// Empty result carries an explicit sentinel, never a bare empty
    /// string — the ambiguity that made a real empty glob look broken.
    #[tokio::test]
    async fn empty_result_reports_no_files_found() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "")
            .await
            .unwrap();

        let out = GlobTool
            .execute(
                json!({ "pattern": "*.nomatch", "path": dir.path() }),
                &ctx(),
            )
            .await
            .unwrap();
        let ToolOutput::Text(s) = out else {
            panic!();
        };
        assert_eq!(s, NO_MATCHES_MESSAGE);
    }
}
