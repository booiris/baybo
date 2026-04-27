//! `Grep` — regex content search across files.
//!
//! This is a minimal implementation that walks the tree with [`walkdir`] and
//! matches each line with the [`regex`] crate. It honors neither `.gitignore`
//! nor file-type filters as richly as ripgrep does; a follow-up can swap in
//! the `grep`/`ignore` crates once we know we need that throughput.

use std::path::PathBuf;

use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

const MAX_HITS: usize = 500;

pub struct GrepTool;

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

    fn description(&self) -> &str {
        "Search file contents with a regular expression. Always use this \
         instead of Bash commands like grep or rg. `output_mode` may be \
         `content` (matching lines), `files_with_matches` (default, paths \
         only), or `count` (match counts per file). Supports file-type \
         filtering via the `glob` parameter.\n\n\
         PATHS: `path` is REQUIRED and MUST be an absolute filesystem path. \
         Relative paths and omission are rejected.\n\n\
         BEFORE SEARCHING: For an unfamiliar directory, first probe its \
         scale with `Glob` (e.g. count entries) and narrow the search root \
         or `glob` filter accordingly. Walking huge unfiltered trees can \
         hang the process."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern":          { "type": "string", "description": "Rust-flavor regex" },
                "path":             { "type": "string", "description": "Absolute directory to search (required)" },
                "glob":             { "type": "string", "description": "Filename glob to filter files (e.g. `*.rs`)" },
                "case_insensitive": { "type": "boolean", "default": false },
                "output_mode":      {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "default": "files_with_matches"
                }
            },
            "required": ["pattern", "path"]
        })
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

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        if !p.path.is_absolute() {
            return Err(ToolError::InvalidParams(format!(
                "Grep `path` must be an absolute path, got `{}`",
                p.path.display()
            )));
        }

        let base = p.path.clone();
        tokio::task::spawn_blocking(move || run_grep(&base, &p))
            .await
            .map_err(|e| ToolError::Execution(format!("join: {e}")))?
    }
}

fn run_grep(base: &std::path::Path, p: &Params) -> crate::Result<ToolOutput> {
    let re = regex::RegexBuilder::new(&p.pattern)
        .case_insensitive(p.case_insensitive)
        .build()
        .map_err(|e| ToolError::InvalidParams(format!("regex: {e}")))?;

    let name_filter: Option<Regex> = match &p.glob {
        Some(g) => Some(
            Regex::new(&glob_to_regex(g))
                .map_err(|e| ToolError::InvalidParams(format!("glob: {e}")))?,
        ),
        None => None,
    };

    let mut files_with_matches: Vec<PathBuf> = Vec::new();
    let mut content_hits: Vec<String> = Vec::new();
    let mut counts: Vec<(PathBuf, usize)> = Vec::new();
    let mut total_hits = 0usize;

    for entry in walkdir::WalkDir::new(base)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if let Some(f) = &name_filter {
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if !f.is_match(name) {
                continue;
            }
        }

        let Ok(contents) = std::fs::read_to_string(path) else {
            continue;
        };

        let mut file_hits = 0usize;
        for (lineno, line) in contents.lines().enumerate() {
            if re.is_match(line) {
                file_hits += 1;
                total_hits += 1;
                if p.output_mode == "content" && content_hits.len() < MAX_HITS {
                    content_hits.push(format!("{}:{}: {}", path.display(), lineno + 1, line));
                }
            }
        }

        if file_hits > 0 {
            if p.output_mode == "files_with_matches" {
                files_with_matches.push(path.to_path_buf());
            } else if p.output_mode == "count" {
                counts.push((path.to_path_buf(), file_hits));
            }
        }
    }

    let body = match p.output_mode.as_str() {
        "content" => {
            let mut s = content_hits.join("\n");
            if total_hits > MAX_HITS {
                s.push_str(&format!("\n… [truncated: {total_hits} total matches]"));
            }
            s
        }
        "count" => counts
            .into_iter()
            .map(|(p, n)| format!("{}: {n}", p.display()))
            .collect::<Vec<_>>()
            .join("\n"),
        "files_with_matches" => files_with_matches
            .into_iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        other => {
            return Err(ToolError::InvalidParams(format!(
                "unknown output_mode `{other}`"
            )));
        }
    };

    Ok(ToolOutput::Text(body))
}

/// Translate a shell-style glob into a regex anchored to the full filename.
/// Supports `*`, `?`, and character classes; not a complete glob dialect.
fn glob_to_regex(glob: &str) -> String {
    let mut re = String::from("^");
    let mut chars = glob.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '\\' => {
                re.push('\\');
                re.push(c);
            }
            '[' => {
                re.push('[');
                for inner in chars.by_ref() {
                    re.push(inner);
                    if inner == ']' {
                        break;
                    }
                }
            }
            _ => re.push(c),
        }
    }
    re.push('$');
    re
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
            subagent: None,
            parent_job_id: None,
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

        let out = GrepTool
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
        let err = GrepTool
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
        let err = GrepTool
            .execute(json!({ "pattern": "x" }), &ctx())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParams(_)),
            "expected InvalidParams, got: {err:?}"
        );
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

        let out = GrepTool
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
}
