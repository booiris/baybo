use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use aura_llm::{ChatRequest, LlmResponse};
use aura_model::ChatMessage;
use aura_workspace::paths::{SESSION_LOG_EXTENSION, sanitize_session_id};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum LlmCallOutcome {
    Ok {
        response: LlmResponseMeta,
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
    pub request: LlmRequestMeta,
    #[serde(flatten)]
    pub outcome: LlmCallOutcome,
}

#[derive(Debug, Clone, Serialize)]
pub struct LlmRequestMeta {
    pub message_count: usize,
    pub message_bytes: usize,
    pub messages_sha256: String,
    pub temperature: Option<f32>,
    pub tools: Vec<String>,
}

impl LlmRequestMeta {
    pub fn from_request(req: &ChatRequest) -> serde_json::Result<Self> {
        let messages = serde_json::to_vec(&req.messages)?;
        Ok(Self {
            message_count: req.messages.len(),
            message_bytes: messages.len(),
            messages_sha256: sha256_hex(&messages),
            temperature: req.temperature,
            tools: req.tools.iter().map(|t| t.name.clone()).collect(),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LlmResponseMeta {
    pub content_bytes: usize,
    pub content_sha256: String,
    pub content_block_count: usize,
    pub tool_call_count: usize,
    pub thinking_bytes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_sha256: Option<String>,
    pub usage: aura_llm::TokenUsage,
}

impl LlmResponseMeta {
    pub fn from_response(response: &LlmResponse) -> Self {
        let content = response.content.as_bytes();
        let thinking = response.thinking.as_ref().map(|value| value.as_bytes());
        Self {
            content_bytes: content.len(),
            content_sha256: sha256_hex(content),
            content_block_count: response.content_blocks.len(),
            tool_call_count: response.tool_calls.len(),
            thinking_bytes: thinking.map_or(0, <[u8]>::len),
            thinking_sha256: thinking.map(sha256_hex),
            usage: response.usage,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionMessageRecord {
    pub timestamp: DateTime<Utc>,
    pub session_id: String,
    pub message_id: String,
    pub parent_id: Option<String>,
    pub message: ChatMessage,
}

#[derive(Default)]
struct SessionLogState {
    last_message_id_by_session: HashMap<String, String>,
}

/// Append-only JSONL writer for per-session events.
///
/// One file per session at `<base_dir>/<sanitized_session_id>.jsonl`. The
/// writer holds a process-local mutex so concurrent calls within the same
/// process serialise their writes — multiple OS processes pointed at the
/// same dir will still interleave at the line boundary because each
/// `log()` issues a single `write_all` for the line + newline.
pub struct SessionLlmLogger {
    base_dir: PathBuf,
    state: Mutex<SessionLogState>,
}

impl SessionLlmLogger {
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            state: Mutex::new(SessionLogState::default()),
        }
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    pub async fn log(&self, record: &LlmCallRecord) -> io::Result<()> {
        self.log_llm_call(record).await
    }

    pub async fn log_llm_call(&self, record: &LlmCallRecord) -> io::Result<()> {
        let mut line = typed_record_line("llm_call", record)?;
        let _guard = self.state.lock().await;
        self.write_line(&record.session_id, &mut line).await
    }

    pub async fn log_message(&self, session_id: &str, message: &ChatMessage) -> io::Result<String> {
        let mut state = self.state.lock().await;
        let parent_id = state.last_message_id_by_session.get(session_id).cloned();
        let message_id = Uuid::new_v4().to_string();
        let record = SessionMessageRecord {
            timestamp: Utc::now(),
            session_id: session_id.to_string(),
            message_id: message_id.clone(),
            parent_id,
            message: message.clone(),
        };
        let mut line = typed_record_line("message", &record)?;
        self.write_line(&record.session_id, &mut line).await?;
        state
            .last_message_id_by_session
            .insert(session_id.to_string(), message_id.clone());
        Ok(message_id)
    }

    async fn write_line(&self, session_id: &str, line: &mut Vec<u8>) -> io::Result<()> {
        let safe_id = sanitize_session_id(session_id);
        let path = self
            .base_dir
            .join(format!("{safe_id}.{SESSION_LOG_EXTENSION}"));
        line.push(b'\n');

        fs::create_dir_all(&self.base_dir).await?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        file.write_all(line).await?;
        file.flush().await?;
        Ok(())
    }
}

fn typed_record_line<T: Serialize>(record_type: &'static str, record: &T) -> io::Result<Vec<u8>> {
    #[derive(Serialize)]
    struct TypedRecord<'a, T: Serialize> {
        #[serde(rename = "type")]
        record_type: &'static str,
        #[serde(flatten)]
        record: &'a T,
    }

    serde_json::to_vec(&TypedRecord {
        record_type,
        record,
    })
    .map_err(io::Error::other)
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
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
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
            thinking: None,
        }
    }

    fn sample_request_meta() -> LlmRequestMeta {
        LlmRequestMeta::from_request(&sample_request()).unwrap()
    }

    fn sample_response_meta() -> LlmResponseMeta {
        LlmResponseMeta::from_response(&sample_response())
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
            request: sample_request_meta(),
            outcome: LlmCallOutcome::Ok {
                response: sample_response_meta(),
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
            assert_eq!(v["type"], "llm_call");
            assert_eq!(v["session_id"], "sess-1");
            assert_eq!(v["model"], "gpt-x");
            assert_eq!(v["outcome"], "ok");
            assert_eq!(v["request"]["temperature"], 0.7);
            assert_eq!(v["request"]["tools"], serde_json::json!(["noop"]));
            assert_eq!(v["request"]["message_count"], 1);
            assert!(v["request"]["messages"].is_null());
            assert!(v["request"]["messages_sha256"].as_str().unwrap().len() == 64);
            assert_eq!(v["response"]["usage"]["input_tokens"], 4);
        }
    }

    #[tokio::test]
    async fn appends_message_records_to_same_session_file() {
        let tmp = tempdir().unwrap();
        let logger = SessionLlmLogger::new(tmp.path().to_path_buf());
        let first = ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text("hello".into())],
        };
        let second = ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::Text("hi".into())],
        };

        let first_id = logger.log_message("sess-1", &first).await.unwrap();
        let second_id = logger.log_message("sess-1", &second).await.unwrap();

        let raw = tokio::fs::read_to_string(tmp.path().join("sess-1.jsonl"))
            .await
            .unwrap();
        let lines: Vec<serde_json::Value> = raw
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["type"], "message");
        assert_eq!(lines[0]["message_id"], first_id);
        assert!(lines[0]["parent_id"].is_null());
        assert_eq!(lines[0]["message"]["role"], "user");
        assert_eq!(lines[1]["message_id"], second_id);
        assert_eq!(lines[1]["parent_id"], first_id);
        assert_eq!(lines[1]["message"]["role"], "assistant");
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
            request: sample_request_meta(),
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
            request: sample_request_meta(),
            outcome: LlmCallOutcome::Ok {
                response: sample_response_meta(),
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
