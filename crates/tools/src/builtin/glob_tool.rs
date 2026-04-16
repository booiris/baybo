//! `Glob` — file pattern matching, results sorted by mtime (newest first).

use std::path::PathBuf;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

const MAX_RESULTS: usize = 1000;

pub struct GlobTool;

#[derive(Debug, Deserialize)]
struct Params {
    pattern: String,
    #[serde(default)]
    path: Option<PathBuf>,
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &str {
        "Find files by glob pattern (e.g. `**/*.rs`). Always use this \
         instead of Bash commands like find or ls for file searching. \
         Results are sorted by modification time, newest first, and \
         capped at 1000 entries."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern to match" },
                "path":    { "type": "string", "description": "Directory to search in (defaults to the current working directory)" }
            },
            "required": ["pattern"]
        })
    }

    fn accessed_resources(&self, params: &Value) -> Vec<ResourceAccess> {
        // Glob enumerates filenames within a directory; treat the search root
        // as a read access so directory approvals cover subsequent reads.
        let base = params
            .get("path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok());
        match base {
            Some(p) => vec![ResourceAccess::ReadFile { path: p }],
            None => Vec::new(),
        }
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        let base = match p.path {
            Some(path) => path,
            None => {
                std::env::current_dir().map_err(|e| ToolError::Execution(format!("cwd: {e}")))?
            }
        };

        tokio::task::spawn_blocking(move || run_glob(&base, &p.pattern))
            .await
            .map_err(|e| ToolError::Execution(format!("join: {e}")))?
    }
}

fn run_glob(base: &std::path::Path, pattern: &str) -> crate::Result<ToolOutput> {
    let full_pattern = if std::path::Path::new(pattern).is_absolute() {
        pattern.to_string()
    } else {
        base.join(pattern).to_string_lossy().into_owned()
    };

    let paths = glob::glob(&full_pattern)
        .map_err(|e| ToolError::InvalidParams(format!("glob pattern: {e}")))?;

    let mut entries: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
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
    }

    entries.sort_by(|a, b| b.1.cmp(&a.1));
    let truncated = entries.len() > MAX_RESULTS;
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
