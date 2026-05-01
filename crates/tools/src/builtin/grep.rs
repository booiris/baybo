//! `Grep` — regex content search across files.
//!
//! This is a minimal implementation that walks the tree with [`walkdir`] and
//! matches each line with the [`regex`] crate. It honors neither `.gitignore`
//! nor file-type filters as richly as ripgrep does; a follow-up can swap in
//! the `grep`/`ignore` crates once we know we need that throughput.

use std::path::PathBuf;
use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

use super::paths::require_absolute;
use crate::{ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

const MAX_HITS: usize = 500;
const MAX_FILES_VISITED: usize = 50_000;
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_FILE_MIB: u64 = MAX_FILE_BYTES / 1024 / 1024;

static DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Search file contents with a regular expression. Always use this \
         instead of Bash commands like grep or rg. `output_mode` may be \
         `content` (matching lines), `files_with_matches` (default, paths \
         only), or `count` (match counts per file). Supports file-type \
         filtering via the `glob` parameter. Sensitive paths (SSH/AWS/GPG \
         configs, .env, /etc/shadow, …) are pruned from the walk so their \
         contents never enter the result.\n\n\
         PATHS: `path` is REQUIRED and MUST be an absolute filesystem path. \
         Relative paths and omission are rejected.\n\n\
         BEFORE SEARCHING: For an unfamiliar directory, first probe its \
         scale with `Glob` (e.g. count entries) and narrow the search root \
         or `glob` filter accordingly. Walks stop after {MAX_FILES_VISITED} \
         files visited, individual files larger than {MAX_FILE_MIB} MiB are \
         skipped, and per-mode results are capped at {MAX_HITS}."
    )
});

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
        &DESCRIPTION
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

        require_absolute(&p.path, "Grep", "path")?;

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
    let mut visited = 0usize;
    let mut walk_truncated = false;

    let walker = walkdir::WalkDir::new(base)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !aura_security::is_sensitive_path(e.path()));

    for entry in walker.filter_map(Result::ok) {
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

        visited += 1;
        if visited > MAX_FILES_VISITED {
            walk_truncated = true;
            break;
        }

        if let Ok(meta) = path.metadata()
            && meta.len() > MAX_FILE_BYTES
        {
            continue;
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
                if files_with_matches.len() < MAX_HITS {
                    files_with_matches.push(path.to_path_buf());
                }
            } else if p.output_mode == "count" && counts.len() < MAX_HITS {
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
            if walk_truncated {
                s.push_str(&format!(
                    "\n… [walk truncated after {MAX_FILES_VISITED} files visited]"
                ));
            }
            s
        }
        "count" => {
            let mut s = counts
                .into_iter()
                .map(|(p, n)| format!("{}: {n}", p.display()))
                .collect::<Vec<_>>()
                .join("\n");
            if walk_truncated {
                s.push_str(&format!(
                    "\n… [walk truncated after {MAX_FILES_VISITED} files visited]"
                ));
            }
            s
        }
        "files_with_matches" => {
            let mut s = files_with_matches
                .into_iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            if walk_truncated {
                s.push_str(&format!(
                    "\n… [walk truncated after {MAX_FILES_VISITED} files visited]"
                ));
            }
            s
        }
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

        let out = GrepTool
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
