//! `Glob` — file pattern matching, results sorted by mtime (newest first).

use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::paths::require_absolute;
use crate::{ResourceAccess, Tool, ToolConcurrency, ToolContext, ToolError, ToolOutput};

const MAX_RESULTS: usize = 1000;
const SCAN_CAP: usize = MAX_RESULTS * 2;

static DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Find files by glob pattern (e.g. `**/*.rs`). Always use this \
         instead of Bash commands like find or ls for file searching. \
         Results are sorted by modification time, newest first, and \
         capped at {MAX_RESULTS} entries.\n\n\
         PATHS: `path` is REQUIRED and MUST be an absolute filesystem path. \
         Relative paths and omission are rejected. `pattern` MUST be \
         relative — absolute patterns (e.g. `/etc/**/*`) are rejected so \
         the search root is always the declared `path`.\n\n\
         BEFORE BROAD PATTERNS: When pointing at an unfamiliar directory, \
         start with a narrow pattern (e.g. top-level `*` or `*/*.rs`) to \
         gauge the file count before issuing recursive `**/*` walks."
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
                "pattern": { "type": "string", "description": "Glob pattern to match" },
                "path":    { "type": "string", "description": "Absolute directory to search in (required)" }
            },
            "required": ["pattern", "path"]
        })
    }

    fn max_timeout(&self) -> Duration {
        // Recursive walks against large monorepos can blow past 30 s
        // even with the SCAN_CAP early-exit; 60 s gives headroom while
        // still bounding a runaway pattern.
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

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        require_absolute(&p.path, "Glob", "path")?;

        if std::path::Path::new(&p.pattern).is_absolute() {
            return Err(ToolError::InvalidParams(format!(
                "Glob `pattern` must be relative to `path`, got absolute `{}`",
                p.pattern
            )));
        }

        let base = p.path;
        tokio::task::spawn_blocking(move || run_glob(&base, &p.pattern))
            .await
            .map_err(|e| ToolError::Execution(format!("join: {e}")))?
    }
}

fn run_glob(base: &std::path::Path, pattern: &str) -> crate::Result<ToolOutput> {
    let full_pattern = base.join(pattern).to_string_lossy().into_owned();

    let paths = glob::glob(&full_pattern)
        .map_err(|e| ToolError::InvalidParams(format!("glob pattern: {e}")))?;

    let mut entries: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    let mut truncated = false;
    for entry in paths {
        let path = match entry {
            Ok(p) => p,
            Err(_) => continue,
        };
        let Ok(meta) = path.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        entries.push((path, mtime));
        if entries.len() >= SCAN_CAP {
            truncated = true;
            break;
        }
    }

    entries.sort_by(|a, b| b.1.cmp(&a.1));
    if entries.len() > MAX_RESULTS {
        truncated = true;
    }
    entries.truncate(MAX_RESULTS);

    let mut lines: Vec<String> = entries
        .into_iter()
        .map(|(p, _)| p.to_string_lossy().into_owned())
        .collect();
    if truncated {
        lines.push(format!("… [truncated to {MAX_RESULTS}]"));
    }

    Ok(ToolOutput::Text(lines.join("\n")))
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
}
