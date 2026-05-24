//! `gemini` external agent — drives Google's `gemini` CLI in non-
//! interactive `--output-format stream-json` mode.
//!
//! Wire protocol (observed against gemini-cli 0.42, one JSON object per
//! line). Differs from both claude_cli and codex_cli:
//!   - prompt is passed via the `-p <prompt>` flag (argv), not stdin
//!     (claude) or a positional after `--` (codex)
//!   - session-id event is `{"type":"init","session_id":"<uuid>",...}`
//!   - assistant text arrives as `{"type":"message","role":"assistant",
//!     "content":"...","delta":true}` — incremental deltas, so we
//!     accumulate them (claude/codex send whole turns)
//!   - tool calls are top-level `{"type":"tool_use",...}` /
//!     `{"type":"tool_result",...}` events (not content blocks)
//!   - the terminal `{"type":"result",...}` carries `stats` but NO
//!     final answer text — FinalContent is built from accumulated
//!     assistant deltas
//!   - `stats.input_tokens` is the TOTAL prompt and `stats.cached` is a
//!     subset of it (OpenAI/Gemini convention, not claude's disjoint
//!     cache buckets)
//!   - resume takes the session uuid directly (`--resume <uuid>`)
//!
//! Security: `--yolo` (auto-approve every tool) plus `--skip-trust`
//! (trust the workspace cwd so YOLO isn't downgraded to interactive
//! approval in an untrusted folder) are hardcoded — gemini's
//! interactive approval prompts can't reach a non-TTY subprocess.
//! aura's sandbox / sensitive_paths / approval gate do NOT apply to
//! gemini's internal tool calls. The workspace cwd pins gemini's
//! default working area but does not constrain its absolute-path reach.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aura_llm::TokenUsage;
use aura_model::{ChatMessage, ContentBlock, ExternalAgentKind};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use super::probe::{
    KILL_GRACE, check_binary_runs, ensure_workspace_dir, reap_after_stream_close, resolve_binary,
    spawn_stderr_tee, take_child_io,
};
use super::{
    ExternalAgent, ExternalAgentError, ExternalAgentEvent, ExternalAgentRequest,
    ExternalAgentStream, Result,
};

const AGENT_NAME: &str = "gemini";

const INSTALL_HINT: &str = "Install the Gemini CLI (npm install -g @google/gemini-cli) or configure an explicit binary path.";

/// Prefix attached to gemini's tool names when projected into aura's
/// `ContentBlock::ToolUse`, so a transcript reader can tell at a glance
/// these are gemini-internal calls that did NOT route through aura's
/// tool registry / approval gate (mirrors codex_cli's `codex_` prefix).
const GEMINI_TOOL_PREFIX: &str = "gemini_";

#[derive(Debug)]
pub struct GeminiCliAgent {
    binary_path: PathBuf,
    /// `--model <NAME>` override. Empty = let gemini pick (it
    /// auto-routes, e.g. `auto-gemini-3`).
    model: String,
}

impl GeminiCliAgent {
    /// Resolve the binary + run `gemini --version`. Same fail-fast shape
    /// as claude_cli / codex_cli.
    pub fn probe_and_build(binary_path: Option<&str>) -> Result<Arc<Self>> {
        let resolved = resolve_binary(
            binary_path,
            ExternalAgentKind::Gemini.binary_name(),
            INSTALL_HINT,
        )?;
        check_binary_runs(&resolved, ExternalAgentKind::Gemini.binary_name())?;
        Ok(Arc::new(Self {
            binary_path: resolved,
            model: String::new(),
        }))
    }

    pub fn binary_path(&self) -> &Path {
        &self.binary_path
    }
}

#[async_trait]
impl ExternalAgent for GeminiCliAgent {
    fn kind(&self) -> ExternalAgentKind {
        ExternalAgentKind::Gemini
    }

    async fn run(&self, request: ExternalAgentRequest) -> Result<ExternalAgentStream> {
        ensure_workspace_dir(&request.workspace_dir, AGENT_NAME).await?;

        let child = self
            .spawn_gemini(
                &request.workspace_dir,
                request.resume_key.as_deref(),
                &request.task,
            )
            .await?;
        spawn_stream_parser(
            child,
            request.cancel.clone(),
            request.timeout,
            request.resume_key.is_none(),
        )
    }
}

impl GeminiCliAgent {
    async fn spawn_gemini(
        &self,
        workspace_dir: &Path,
        resume_id: Option<&str>,
        prompt: &str,
    ) -> Result<Child> {
        let mut cmd = Command::new(&self.binary_path);
        cmd.arg("--output-format")
            .arg("stream-json")
            .arg("--yolo")
            .arg("--skip-trust");
        if !self.model.is_empty() {
            cmd.arg("--model").arg(&self.model);
        }
        if let Some(id) = resume_id {
            cmd.arg("--resume").arg(id);
        }
        // `-p` carries the prompt as a single argv value (no shell
        // interpretation), so a prompt starting with `-` is safe.
        cmd.arg("-p").arg(prompt);

        cmd.current_dir(workspace_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        cmd.spawn().map_err(|e| {
            ExternalAgentError::Config(format!(
                "gemini: failed to spawn `{}`: {e}",
                self.binary_path.display()
            ))
        })
    }
}

fn spawn_stream_parser(
    mut child: Child,
    cancel: tokio_util::sync::CancellationToken,
    timeout: Duration,
    is_fresh_session: bool,
) -> Result<ExternalAgentStream> {
    let (stdout, stderr) = take_child_io(&mut child, AGENT_NAME)?;
    let stderr_buf = spawn_stderr_tee(stderr, AGENT_NAME);

    let stream = async_stream::stream! {
        let mut reader = BufReader::new(stdout).lines();
        let mut session_persisted = false;
        // All assistant text, for FinalContent.
        let mut accumulated = String::new();
        // Consecutive assistant deltas awaiting a transcript flush —
        // grouped into one Intermediate row so a token-streamed answer
        // doesn't fragment into hundreds of rows, while text emitted
        // between tool calls stays correctly ordered.
        let mut pending_text = String::new();
        let mut buffered_usage: Option<TokenUsage> = None;
        let mut deadline = tokio::time::Instant::now() + timeout;

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    let _ = child.start_kill();
                    let _ = tokio::time::timeout(KILL_GRACE, child.wait()).await;
                    yield Err(ExternalAgentError::Transient("gemini: cancelled by parent".into()));
                    return;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let _ = child.start_kill();
                    let _ = tokio::time::timeout(KILL_GRACE, child.wait()).await;
                    yield Err(ExternalAgentError::Transient(
                        "gemini: idle timeout — no output within the safety window".into()
                    ));
                    return;
                }
                line_result = reader.next_line() => {
                    let line = match line_result {
                        Ok(Some(l)) => l,
                        Ok(None) => break,
                        Err(e) => {
                            yield Err(ExternalAgentError::Transient(format!(
                                "gemini: read stdout: {e}"
                            )));
                            return;
                        }
                    };
                    // Any output means the subprocess is alive — reset the
                    // idle timer so only a genuinely silent run is killed.
                    deadline = tokio::time::Instant::now() + timeout;
                    if line.is_empty() {
                        continue;
                    }
                    let Ok(event) = serde_json::from_str::<GeminiEvent>(&line) else {
                        // Forward-compat: unknown line shape, skip.
                        continue;
                    };
                    match event {
                        GeminiEvent::Init { session_id: Some(id) }
                            if is_fresh_session && !session_persisted =>
                        {
                            session_persisted = true;
                            yield Ok(ExternalAgentEvent::ResumeKey(id));
                        }
                        GeminiEvent::Message { role: Role::Assistant, content } => {
                            if !content.is_empty() {
                                accumulated.push_str(&content);
                                pending_text.push_str(&content);
                                yield Ok(ExternalAgentEvent::TextDelta(content));
                            }
                        }
                        // role:user echoes the prompt we already persisted
                        // as the child's first message — skip it.
                        GeminiEvent::Message { role: Role::User, .. } => {}
                        GeminiEvent::ToolUse { tool_name, tool_id, parameters } => {
                            if !pending_text.is_empty() {
                                yield Ok(ExternalAgentEvent::Intermediate(ChatMessage::assistant(
                                    vec![ContentBlock::Text(std::mem::take(&mut pending_text))],
                                )));
                            }
                            yield Ok(ExternalAgentEvent::Intermediate(ChatMessage::assistant(
                                vec![ContentBlock::ToolUse {
                                    id: tool_id,
                                    name: format!("{GEMINI_TOOL_PREFIX}{tool_name}"),
                                    input: parameters,
                                    signature: None,
                                }],
                            )));
                        }
                        GeminiEvent::ToolResult { tool_id, status, output } => {
                            if !pending_text.is_empty() {
                                yield Ok(ExternalAgentEvent::Intermediate(ChatMessage::assistant(
                                    vec![ContentBlock::Text(std::mem::take(&mut pending_text))],
                                )));
                            }
                            // Tools that produce no textual output (e.g. a
                            // bare success ack) still get a row carrying
                            // the status so the pairing is visible.
                            let content = output
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| format!("status: {status}"));
                            yield Ok(ExternalAgentEvent::Intermediate(
                                ChatMessage::tool_result(tool_id, content),
                            ));
                        }
                        GeminiEvent::Result { status, error, stats } => {
                            if status != ResultStatus::Success {
                                let detail = error
                                    .map(|e| e.message)
                                    .filter(|m| !m.is_empty())
                                    .unwrap_or_else(|| stderr_buf.lock().trim().to_string());
                                yield Err(classify_gemini_error(&detail));
                                return;
                            }
                            buffered_usage = stats.map(GeminiStats::into_token_usage);
                            // Hold FinalContent / Usage until after stdout
                            // EOF + reap so a consumer that breaks on
                            // FinalContent doesn't drop the stream while
                            // gemini is still flushing.
                        }
                        // Init without a session_id, or any unknown event: ignore.
                        _ => {}
                    }
                }
            }
        }
        if let Err(e) = reap_after_stream_close(&mut child, AGENT_NAME, &stderr_buf).await {
            yield Err(e);
            return;
        }
        if !pending_text.is_empty() {
            yield Ok(ExternalAgentEvent::Intermediate(ChatMessage::assistant(
                vec![ContentBlock::Text(std::mem::take(&mut pending_text))],
            )));
        }
        if let Some(u) = buffered_usage {
            yield Ok(ExternalAgentEvent::Usage(u));
        }
        let blocks = if accumulated.is_empty() {
            Vec::new()
        } else {
            vec![ContentBlock::Text(accumulated)]
        };
        yield Ok(ExternalAgentEvent::FinalContent(blocks));
    };

    Ok(Box::pin(stream))
}

/// `Config` (operator must act) vs `Transient` (retry might help).
fn classify_gemini_error(detail: &str) -> ExternalAgentError {
    let lowered = detail.to_ascii_lowercase();
    let auth_hit = lowered.contains("logged out")
        || lowered.contains("not logged in")
        || lowered.contains("authenticate")
        || lowered.contains("unauthenticated")
        || lowered.contains("credential")
        || lowered.contains("permission denied");
    if auth_hit {
        return ExternalAgentError::Config(format!(
            "gemini: not authenticated — run `gemini` once interactively to sign in, or set \
             GEMINI_API_KEY. Detail: {detail}"
        ));
    }
    let rate_hit = lowered.contains("rate limit")
        || lowered.contains("quota")
        || lowered.contains("resource_exhausted")
        || lowered.contains("too many requests");
    if rate_hit {
        return ExternalAgentError::Transient(format!("gemini: rate limited: {detail}"));
    }
    ExternalAgentError::Transient(format!("gemini: {detail}"))
}

// ---------------------------------------------------------------------------
// stream-json event shapes (gemini-cli `--output-format stream-json`)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum GeminiEvent {
    Init {
        #[serde(default)]
        session_id: Option<String>,
    },
    Message {
        role: Role,
        #[serde(default)]
        content: String,
    },
    ToolUse {
        tool_name: String,
        tool_id: String,
        #[serde(default)]
        parameters: serde_json::Value,
    },
    ToolResult {
        tool_id: String,
        #[serde(default)]
        status: String,
        #[serde(default)]
        output: Option<String>,
    },
    Result {
        #[serde(default)]
        status: ResultStatus,
        #[serde(default)]
        error: Option<GeminiError>,
        #[serde(default)]
        stats: Option<GeminiStats>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Role {
    User,
    Assistant,
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ResultStatus {
    Success,
    #[default]
    #[serde(other)]
    Error,
}

#[derive(Debug, Deserialize)]
struct GeminiError {
    #[serde(default)]
    message: String,
}

/// `result.stats` token counters. `input_tokens` is the full prompt
/// (fresh + cached) and `cached` is the cached SUBSET of it — the
/// OpenAI/Gemini convention, so no bucket-folding is needed (unlike
/// claude's disjoint cache_read/cache_creation split).
#[derive(Debug, Deserialize)]
struct GeminiStats {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(default)]
    cached: i64,
}

impl GeminiStats {
    fn into_token_usage(self) -> TokenUsage {
        TokenUsage {
            input_tokens: clamp_to_usize(self.input_tokens),
            output_tokens: clamp_to_usize(self.output_tokens),
            cached_input_tokens: clamp_to_usize(self.cached),
            cache_creation_input_tokens: 0,
        }
    }
}

fn clamp_to_usize(n: i64) -> usize {
    if n < 0 { 0 } else { n as usize }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_agent::probe::test_helpers::fake_binary;

    #[test]
    fn probe_succeeds_with_working_binary() {
        let (_dir, bin) = fake_binary("gemini", "0.42.0");
        let agent = GeminiCliAgent::probe_and_build(Some(bin.to_str().unwrap()))
            .expect("probe should succeed");
        assert_eq!(agent.kind(), ExternalAgentKind::Gemini);
    }

    #[test]
    fn probe_fails_when_binary_path_missing() {
        let err = GeminiCliAgent::probe_and_build(Some("/nonexistent/gemini-binary-xyzzy"))
            .expect_err("expected error for missing binary path");
        match err {
            ExternalAgentError::NotInstalled(msg) => {
                assert!(msg.contains("does not exist"), "msg: {msg}");
                assert!(msg.contains("xyzzy"), "msg: {msg}");
            }
            other => panic!("expected NotInstalled, got {other:?}"),
        }
    }

    #[test]
    fn parse_init_event_extracts_session_id() {
        let line = r#"{"type":"init","timestamp":"t","session_id":"6160f46b-uuid","model":"auto-gemini-3"}"#;
        match serde_json::from_str::<GeminiEvent>(line).unwrap() {
            GeminiEvent::Init {
                session_id: Some(id),
            } => assert_eq!(id, "6160f46b-uuid"),
            other => panic!("expected Init with session_id, got {other:?}"),
        }
    }

    #[test]
    fn parse_assistant_message_delta() {
        let line = r#"{"type":"message","timestamp":"t","role":"assistant","content":"DONE","delta":true}"#;
        match serde_json::from_str::<GeminiEvent>(line).unwrap() {
            GeminiEvent::Message {
                role: Role::Assistant,
                content,
            } => assert_eq!(content, "DONE"),
            other => panic!("expected assistant Message, got {other:?}"),
        }
    }

    #[test]
    fn parse_user_message_is_distinct_role() {
        let line = r#"{"type":"message","role":"user","content":"do the thing"}"#;
        match serde_json::from_str::<GeminiEvent>(line).unwrap() {
            GeminiEvent::Message {
                role: Role::User, ..
            } => {}
            other => panic!("expected user Message, got {other:?}"),
        }
    }

    #[test]
    fn parse_tool_use_event() {
        let line = r#"{"type":"tool_use","tool_name":"run_shell_command","tool_id":"run_shell_command_123_1","parameters":{"command":"echo hi"}}"#;
        match serde_json::from_str::<GeminiEvent>(line).unwrap() {
            GeminiEvent::ToolUse {
                tool_name,
                tool_id,
                parameters,
            } => {
                assert_eq!(tool_name, "run_shell_command");
                assert_eq!(tool_id, "run_shell_command_123_1");
                assert_eq!(parameters["command"], "echo hi");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn parse_tool_result_with_and_without_output() {
        let with = r#"{"type":"tool_result","tool_id":"t1","status":"success","output":"hello\n"}"#;
        match serde_json::from_str::<GeminiEvent>(with).unwrap() {
            GeminiEvent::ToolResult {
                tool_id,
                status,
                output,
            } => {
                assert_eq!(tool_id, "t1");
                assert_eq!(status, "success");
                assert_eq!(output.as_deref(), Some("hello\n"));
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
        let without = r#"{"type":"tool_result","tool_id":"t2","status":"success"}"#;
        match serde_json::from_str::<GeminiEvent>(without).unwrap() {
            GeminiEvent::ToolResult { output, .. } => assert!(output.is_none()),
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn parse_result_success_with_stats() {
        let line = r#"{"type":"result","status":"success","stats":{"total_tokens":19057,"input_tokens":18781,"output_tokens":157,"cached":7508,"input":11273}}"#;
        match serde_json::from_str::<GeminiEvent>(line).unwrap() {
            GeminiEvent::Result {
                status: ResultStatus::Success,
                stats: Some(s),
                ..
            } => {
                let tu = s.into_token_usage();
                // input_tokens is the total prompt; cached is a subset.
                assert_eq!(tu.input_tokens, 18781);
                assert_eq!(tu.output_tokens, 157);
                assert_eq!(tu.cached_input_tokens, 7508);
                assert_eq!(tu.cache_creation_input_tokens, 0);
            }
            other => panic!("expected Result success with stats, got {other:?}"),
        }
    }

    #[test]
    fn parse_result_error_carries_message() {
        let line = r#"{"type":"result","status":"error","error":{"type":"unknown","message":"[API Error: Requested entity was not found.]"},"stats":{"input_tokens":0,"output_tokens":0,"cached":0}}"#;
        match serde_json::from_str::<GeminiEvent>(line).unwrap() {
            GeminiEvent::Result {
                status: ResultStatus::Error,
                error: Some(e),
                ..
            } => assert!(e.message.contains("Requested entity was not found")),
            other => panic!("expected Result error, got {other:?}"),
        }
    }

    #[test]
    fn parse_unknown_event_falls_to_other() {
        let line = r#"{"type":"some_future_event","payload":{}}"#;
        assert!(matches!(
            serde_json::from_str::<GeminiEvent>(line).unwrap(),
            GeminiEvent::Other
        ));
    }

    #[test]
    fn classify_auth_error_is_config() {
        let e = classify_gemini_error("Request had invalid authentication credentials");
        match e {
            ExternalAgentError::Config(msg) => {
                assert!(msg.contains("not authenticated"), "msg: {msg}")
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn classify_rate_limit_is_transient() {
        let e = classify_gemini_error("RESOURCE_EXHAUSTED: quota exceeded");
        match e {
            ExternalAgentError::Transient(msg) => {
                assert!(msg.contains("rate limited"), "msg: {msg}")
            }
            other => panic!("expected Transient, got {other:?}"),
        }
    }

    #[test]
    fn clamp_negative_to_zero() {
        assert_eq!(clamp_to_usize(-5), 0);
        assert_eq!(clamp_to_usize(0), 0);
        assert_eq!(clamp_to_usize(42), 42);
    }
}
