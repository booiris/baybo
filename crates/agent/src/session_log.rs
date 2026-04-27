use std::io;
use std::path::{Path, PathBuf};

use aura_llm::{ChatRequest, LlmResponse};
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

const FILE_EXTENSION: &str = "jsonl";

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum LlmCallOutcome {
    Ok {
        response: LlmResponse,
        latency_ms: u64,
    },
    Err {
        error: String,
        latency_ms: u64,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct LlmCallRecord {
    pub timestamp: DateTime<Utc>,
    pub session_id: String,
    pub provider: String,
    pub model: String,
    pub request: ChatRequest,
    #[serde(flatten)]
    pub outcome: LlmCallOutcome,
}

/// Append-only JSONL writer for per-session LLM call traces.
///
/// One file per session at `<base_dir>/<sanitized_session_id>.jsonl`. The
/// writer holds a process-local mutex so concurrent calls within the same
/// process serialise their writes — multiple OS processes pointed at the
/// same dir will still interleave at the line boundary because each
/// `log()` issues a single `write_all` for the line + newline.
pub struct SessionLlmLogger {
    base_dir: PathBuf,
    write_lock: Mutex<()>,
}

impl SessionLlmLogger {
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            write_lock: Mutex::new(()),
        }
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    pub async fn log(&self, record: &LlmCallRecord) -> io::Result<()> {
        let safe_id = sanitize_session_id(&record.session_id);
        let path = self.base_dir.join(format!("{safe_id}.{FILE_EXTENSION}"));
        let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
        line.push(b'\n');

        let _guard = self.write_lock.lock().await;
        fs::create_dir_all(&self.base_dir).await?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        file.write_all(&line).await?;
        file.flush().await?;
        Ok(())
    }
}

/// Map a session id to a filesystem-safe stem. Path separators, NULs,
/// leading dots, and any character outside `[A-Za-z0-9_.-]` are replaced
/// with `_` so a hostile or unusual id can never escape `base_dir`.
fn sanitize_session_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for ch in id.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() || out.starts_with('.') {
        out.insert(0, '_');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_llm::{TokenUsage, ToolDefinitionForLlm};
    use aura_model::{ChatMessage, ContentBlock, Role};
    use tempfile::tempdir;

    fn sample_request() -> ChatRequest {
        ChatRequest {
            messages: vec![ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::Text("hello".into())],
            }],
            temperature: Some(0.7),
            tools: vec![ToolDefinitionForLlm {
                name: "noop".into(),
                description: "no-op".into(),
                parameters_schema: serde_json::json!({"type": "object"}),
            }],
        }
    }

    fn sample_response() -> LlmResponse {
        LlmResponse {
            content: "hi there".into(),
            content_blocks: vec![ContentBlock::Text("hi there".into())],
            tool_calls: vec![],
            usage: TokenUsage {
                input_tokens: 4,
                output_tokens: 2,
            },
            thinking: None,
        }
    }

    #[tokio::test]
    async fn appends_one_line_per_call() {
        let tmp = tempdir().unwrap();
        let logger = SessionLlmLogger::new(tmp.path().to_path_buf());

        let record = LlmCallRecord {
            timestamp: Utc::now(),
            session_id: "sess-1".into(),
            provider: "openai".into(),
            model: "gpt-x".into(),
            request: sample_request(),
            outcome: LlmCallOutcome::Ok {
                response: sample_response(),
                latency_ms: 12,
            },
        };

        logger.log(&record).await.unwrap();
        logger.log(&record).await.unwrap();

        let path = tmp.path().join("sess-1.jsonl");
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["session_id"], "sess-1");
            assert_eq!(v["model"], "gpt-x");
            assert_eq!(v["outcome"], "ok");
            assert_eq!(v["request"]["temperature"], 0.7);
            assert_eq!(v["response"]["usage"]["input_tokens"], 4);
        }
    }

    #[tokio::test]
    async fn records_error_outcome() {
        let tmp = tempdir().unwrap();
        let logger = SessionLlmLogger::new(tmp.path().to_path_buf());

        let record = LlmCallRecord {
            timestamp: Utc::now(),
            session_id: "sess-2".into(),
            provider: "anthropic".into(),
            model: "claude-x".into(),
            request: sample_request(),
            outcome: LlmCallOutcome::Err {
                error: "rate limited".into(),
                latency_ms: 50,
            },
        };
        logger.log(&record).await.unwrap();

        let raw = tokio::fs::read_to_string(tmp.path().join("sess-2.jsonl"))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(v["outcome"], "err");
        assert_eq!(v["error"], "rate limited");
    }

    #[tokio::test]
    async fn sanitizes_session_id_path_separators() {
        let tmp = tempdir().unwrap();
        let logger = SessionLlmLogger::new(tmp.path().to_path_buf());

        let record = LlmCallRecord {
            timestamp: Utc::now(),
            session_id: "../etc/passwd".into(),
            provider: "p".into(),
            model: "m".into(),
            request: sample_request(),
            outcome: LlmCallOutcome::Ok {
                response: sample_response(),
                latency_ms: 1,
            },
        };
        logger.log(&record).await.unwrap();

        let mut entries = tokio::fs::read_dir(tmp.path()).await.unwrap();
        let mut names = Vec::new();
        while let Some(e) = entries.next_entry().await.unwrap() {
            names.push(e.file_name().into_string().unwrap());
        }
        assert_eq!(names.len(), 1);
        let name = &names[0];
        assert!(name.ends_with(".jsonl"));
        assert!(!name.contains('/'));
        assert!(!name.starts_with('.'));
    }

    #[test]
    fn sanitize_replaces_unsafe_characters() {
        assert_eq!(sanitize_session_id("abc-123_DEF.4"), "abc-123_DEF.4");
        assert_eq!(sanitize_session_id("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_session_id(""), "_");
        assert_eq!(sanitize_session_id(".hidden"), "_.hidden");
    }
}
