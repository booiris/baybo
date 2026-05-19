//! `claude` external agent — drives the user-installed `claude`
//! binary as a one-shot subprocess.
//!
//! Security: hardcoded `--permission-mode bypassPermissions`. claude's
//! interactive permission gates can't reach a non-TTY subprocess, so
//! we opt into "trust the agent". aura's sandbox / sensitive_paths /
//! approval gate do NOT apply to claude's internal tool calls. The
//! per-spawn `--add-dir <workspace_dir>` only constrains claude's
//! *default* working area, not its absolute-path reach.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use aura_llm::TokenUsage;
use aura_model::{ContentBlock, ExternalAgentKind};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use super::probe::{
    KILL_GRACE, check_binary_runs, ensure_workspace_dir, reap_after_stream_close, resolve_binary,
    spawn_stderr_tee, take_child_io,
};
use super::{
    ExternalAgent, ExternalAgentError, ExternalAgentEvent, ExternalAgentRequest,
    ExternalAgentStream, Result,
};

const AGENT_NAME: &str = "claude";

const PERMISSION_MODE: &str = "bypassPermissions";

/// claude-code's stable alias for the highest-tier model the
/// subscription supports; a date-versioned id would rot.
const DEFAULT_MODEL_FLAG: &str = "opus";

const INSTALL_HINT: &str =
    "Install Claude Code (npm install -g @anthropic-ai/claude-code) or configure an explicit binary path.";

#[derive(Debug)]
pub struct ClaudeCliAgent {
    binary_path: PathBuf,
    model: String,
}

impl ClaudeCliAgent {
    /// Resolve the binary (PATH if `binary_path` is None, otherwise
    /// the explicit path) and run `claude --version` to confirm it
    /// executes. Failure surfaces as `ExternalAgentError::Config`
    /// with operator-actionable text — callers (the boot path) use
    /// this for fail-fast registration.
    pub fn probe_and_build(binary_path: Option<&str>) -> Result<Arc<Self>> {
        let resolved = resolve_binary(
            binary_path,
            ExternalAgentKind::Claude.binary_name(),
            INSTALL_HINT,
        )?;
        check_binary_runs(&resolved, ExternalAgentKind::Claude.binary_name())?;
        Ok(Arc::new(Self {
            binary_path: resolved,
            model: DEFAULT_MODEL_FLAG.to_string(),
        }))
    }

    /// Path to the resolved binary. Useful for setup flows that
    /// probe via PATH and want to record the concrete location.
    pub fn binary_path(&self) -> &Path {
        &self.binary_path
    }
}

#[async_trait]
impl ExternalAgent for ClaudeCliAgent {
    fn kind(&self) -> ExternalAgentKind {
        ExternalAgentKind::Claude
    }

    async fn run(&self, request: ExternalAgentRequest) -> Result<ExternalAgentStream> {
        ensure_workspace_dir(&request.workspace_dir, AGENT_NAME).await?;

        let child = self
            .spawn_claude(
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

impl ClaudeCliAgent {
    async fn spawn_claude(
        &self,
        workspace_dir: &Path,
        resume_id: Option<&str>,
        prompt: &str,
    ) -> Result<Child> {
        let mut cmd = Command::new(&self.binary_path);
        cmd.arg("-p")
            .arg("--output-format")
            .arg("stream-json")
            // claude-code requires --verbose with stream-json.
            .arg("--verbose")
            .arg("--permission-mode")
            .arg(PERMISSION_MODE)
            .arg("--add-dir")
            .arg(workspace_dir);
        if !self.model.is_empty() {
            cmd.arg("--model").arg(&self.model);
        }
        if let Some(id) = resume_id {
            cmd.arg("--resume").arg(id);
        }
        cmd.current_dir(workspace_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            ExternalAgentError::Config(format!(
                "claude: failed to spawn `{}`: {e}",
                self.binary_path.display()
            ))
        })?;
        // claude reads stdin to EOF before starting its agent loop.
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| ExternalAgentError::Transient("claude: stdin not captured".into()))?;
        stdin.write_all(prompt.as_bytes()).await.map_err(|e| {
            ExternalAgentError::Transient(format!("claude: write stdin: {e}"))
        })?;
        drop(stdin);
        Ok(child)
    }
}

fn spawn_stream_parser(
    mut child: Child,
    cancel: tokio_util::sync::CancellationToken,
    timeout: std::time::Duration,
    is_fresh_session: bool,
) -> Result<ExternalAgentStream> {
    let (stdout, stderr) = take_child_io(&mut child, AGENT_NAME)?;
    let stderr_buf = spawn_stderr_tee(stderr, AGENT_NAME);

    let stream = async_stream::stream! {
        let mut reader = BufReader::new(stdout).lines();
        let mut session_persisted = false;
        let mut accumulated = String::new();
        let mut final_emitted = false;
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    let _ = child.start_kill();
                    let _ = tokio::time::timeout(KILL_GRACE, child.wait()).await;
                    yield Err(ExternalAgentError::Transient(
                        "claude: cancelled by parent".into()
                    ));
                    return;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let _ = child.start_kill();
                    let _ = tokio::time::timeout(KILL_GRACE, child.wait()).await;
                    yield Err(ExternalAgentError::Transient(
                        "claude: exceeded declared timeout".into()
                    ));
                    return;
                }
                line_result = reader.next_line() => {
                    let line = match line_result {
                        Ok(Some(l)) => l,
                        Ok(None) => break,
                        Err(e) => {
                            yield Err(ExternalAgentError::Transient(format!(
                                "claude: read stdout: {e}"
                            )));
                            return;
                        }
                    };
                    if line.is_empty() {
                        continue;
                    }
                    // Skip unknown line shapes — claude-code emits
                    // hook lifecycle, rate_limit_event, and other
                    // informational lines between init and result.
                    let Ok(event) = serde_json::from_str::<StreamJsonEvent>(&line) else {
                        continue;
                    };
                    match event {
                        StreamJsonEvent::System {
                            subtype: SystemSubtype::Init,
                            session_id: Some(id),
                        } if is_fresh_session && !session_persisted => {
                            session_persisted = true;
                            yield Ok(ExternalAgentEvent::ResumeKey(id));
                        }
                        StreamJsonEvent::Assistant { message } => {
                            for block in message.content {
                                if let AssistantBlock::Text { text } = block
                                    && !text.is_empty()
                                {
                                    accumulated.push_str(&text);
                                    yield Ok(ExternalAgentEvent::TextDelta(text));
                                }
                            }
                        }
                        StreamJsonEvent::Result {
                            subtype,
                            usage,
                            result,
                            is_error,
                        } => {
                            if is_error.unwrap_or(false) || matches!(subtype, ResultSubtype::Error(_)) {
                                let stderr_snapshot = stderr_buf.lock().clone();
                                let kind = match &subtype {
                                    ResultSubtype::Error(s) => s.as_str(),
                                    ResultSubtype::Success => "unknown",
                                };
                                let detail = result.unwrap_or_else(|| stderr_snapshot.trim().to_string());
                                yield Err(classify_claude_error(kind, &detail));
                                return;
                            }
                            if let Some(u) = usage {
                                yield Ok(ExternalAgentEvent::Usage(u.into_token_usage()));
                            }
                            if !final_emitted {
                                let blocks = if accumulated.is_empty() {
                                    Vec::new()
                                } else {
                                    vec![ContentBlock::Text(accumulated.clone())]
                                };
                                yield Ok(ExternalAgentEvent::FinalContent(blocks));
                                final_emitted = true;
                            }
                        }
                        // claude runs its own tool loop; tool_use /
                        // tool_result / user-echo events aren't surfaced.
                        _ => {}
                    }
                }
            }
        }
        if let Err(e) = reap_after_stream_close(&mut child, AGENT_NAME, &stderr_buf).await {
            yield Err(e);
            return;
        }
        if !final_emitted {
            let blocks = if accumulated.is_empty() {
                Vec::new()
            } else {
                vec![ContentBlock::Text(accumulated.clone())]
            };
            yield Ok(ExternalAgentEvent::FinalContent(blocks));
        }
    };

    Ok(Box::pin(stream))
}

/// `Config` (operator must act) vs `Transient` (retry might help).
fn classify_claude_error(subtype: &str, detail: &str) -> ExternalAgentError {
    let lowered = detail.to_ascii_lowercase();
    let auth_hit = lowered.contains("logged out")
        || lowered.contains("not logged in")
        || lowered.contains("authenticate");
    if auth_hit {
        return ExternalAgentError::Config(format!(
            "claude: not logged in — run `claude login`. Detail: {detail}"
        ));
    }
    let rate_hit = lowered.contains("rate limit")
        || lowered.contains("quota")
        || lowered.contains("usage limit");
    if rate_hit {
        return ExternalAgentError::Transient(format!("claude: rate limited: {detail}"));
    }
    ExternalAgentError::Transient(format!("claude: {subtype}: {detail}"))
}

/// Unknown events deserialize to `Other` so future claude-code event
/// types (hook lifecycle, rate_limit_event, …) don't crash the
/// parser.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamJsonEvent {
    System {
        #[serde(rename = "subtype")]
        subtype: SystemSubtype,
        #[serde(default)]
        session_id: Option<String>,
    },
    Assistant {
        message: AssistantMessage,
    },
    Result {
        subtype: ResultSubtype,
        #[serde(default)]
        usage: Option<StreamJsonUsage>,
        #[serde(default)]
        result: Option<String>,
        #[serde(default)]
        is_error: Option<bool>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SystemSubtype {
    Init,
    #[serde(other)]
    Other,
}

/// `#[serde(untagged)]` mis-routes `"success"` into `Error` (untagged
/// tries each variant; unit `Success` can't take a String, so `Error`
/// wins). Custom `From<String>` mapping is explicit.
#[derive(Debug, Deserialize)]
#[serde(from = "String")]
enum ResultSubtype {
    Success,
    Error(String),
}

impl From<String> for ResultSubtype {
    fn from(s: String) -> Self {
        if s == "success" {
            Self::Success
        } else {
            Self::Error(s)
        }
    }
}

#[derive(Debug, Deserialize)]
struct AssistantMessage {
    #[serde(default)]
    content: Vec<AssistantBlock>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AssistantBlock {
    Text { text: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct StreamJsonUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

impl StreamJsonUsage {
    fn into_token_usage(self) -> TokenUsage {
        // claude reports cache_creation and cache_read as disjoint
        // buckets; fold into `input_tokens` (total prompt) so cost
        // code can use one formula across providers.
        let total_input = self
            .input_tokens
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens);
        TokenUsage {
            input_tokens: total_input as usize,
            output_tokens: self.output_tokens as usize,
            cached_input_tokens: self.cache_read_input_tokens as usize,
            cache_creation_input_tokens: self.cache_creation_input_tokens as usize,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn fake_claude(version: &str) -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("claude");
        std::fs::write(&bin, format!("#!/bin/sh\necho '{version}'\n")).unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();
        (dir, bin)
    }

    #[test]
    fn probe_succeeds_with_working_binary() {
        let (_dir, bin) = fake_claude("claude 1.2.3");
        let agent = ClaudeCliAgent::probe_and_build(Some(bin.to_str().unwrap()))
            .expect("probe should succeed");
        assert_eq!(agent.kind(), ExternalAgentKind::Claude);
    }

    #[test]
    fn probe_fails_when_binary_path_missing() {
        let err = ClaudeCliAgent::probe_and_build(Some("/nonexistent/claude-binary-xyzzy"))
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
    fn probe_fails_when_binary_prints_nothing() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("claude");
        std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();

        let err = ClaudeCliAgent::probe_and_build(Some(bin.to_str().unwrap()))
            .expect_err("must fail");
        match err {
            ExternalAgentError::Config(msg) => {
                assert!(msg.contains("printed nothing"), "msg: {msg}")
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn probe_fails_when_binary_exits_nonzero() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("claude");
        std::fs::write(&bin, "#!/bin/sh\necho 'oops' >&2\nexit 7\n").unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();

        let err = ClaudeCliAgent::probe_and_build(Some(bin.to_str().unwrap()))
            .expect_err("must fail");
        match err {
            ExternalAgentError::Config(msg) => {
                assert!(msg.contains("exited"), "msg: {msg}");
                assert!(msg.contains("oops") || msg.contains("7"), "msg: {msg}");
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn parse_init_event_extracts_session_id() {
        let line = r#"{"type":"system","subtype":"init","session_id":"abc-123","tools":[],"model":"opus"}"#;
        let parsed: StreamJsonEvent = serde_json::from_str(line).unwrap();
        match parsed {
            StreamJsonEvent::System {
                subtype: SystemSubtype::Init,
                session_id: Some(id),
            } => assert_eq!(id, "abc-123"),
            other => panic!("expected System Init with session_id, got {other:?}"),
        }
    }

    #[test]
    fn parse_system_hook_event_falls_to_other_subtype() {
        // claude 2.x emits hook_started / hook_response system events
        // before init. Drop them.
        let line = r#"{"type":"system","subtype":"hook_started","hook_id":"x","hook_name":"y","hook_event":"z","session_id":"s"}"#;
        match serde_json::from_str::<StreamJsonEvent>(line).unwrap() {
            StreamJsonEvent::System {
                subtype: SystemSubtype::Other,
                ..
            } => {}
            other => panic!("expected System Other, got {other:?}"),
        }
    }

    #[test]
    fn parse_assistant_text_event() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}]}}"#;
        match serde_json::from_str::<StreamJsonEvent>(line).unwrap() {
            StreamJsonEvent::Assistant { message } => {
                assert_eq!(message.content.len(), 1);
                match &message.content[0] {
                    AssistantBlock::Text { text } => assert_eq!(text, "hello"),
                    other => panic!("expected Text block, got {other:?}"),
                }
            }
            other => panic!("expected Assistant event, got {other:?}"),
        }
    }

    #[test]
    fn parse_result_event_carries_usage_and_succeeds() {
        let line = r#"{"type":"result","subtype":"success","usage":{"input_tokens":100,"output_tokens":42,"cache_creation_input_tokens":5,"cache_read_input_tokens":10},"result":"ok","is_error":false}"#;
        match serde_json::from_str::<StreamJsonEvent>(line).unwrap() {
            StreamJsonEvent::Result {
                subtype: ResultSubtype::Success,
                usage: Some(u),
                result: Some(r),
                is_error: Some(false),
            } => {
                assert_eq!(r, "ok");
                let tu = u.into_token_usage();
                assert_eq!(tu.input_tokens, 115);
                assert_eq!(tu.output_tokens, 42);
                assert_eq!(tu.cached_input_tokens, 10);
                assert_eq!(tu.cache_creation_input_tokens, 5);
            }
            other => panic!("expected Result Success with usage, got {other:?}"),
        }
    }

    #[test]
    fn parse_result_event_with_error_subtype() {
        let line = r#"{"type":"result","subtype":"error_max_turns","is_error":true,"result":"too many turns"}"#;
        match serde_json::from_str::<StreamJsonEvent>(line).unwrap() {
            StreamJsonEvent::Result {
                subtype: ResultSubtype::Error(kind),
                is_error: Some(true),
                result: Some(r),
                ..
            } => {
                assert_eq!(kind, "error_max_turns");
                assert_eq!(r, "too many turns");
            }
            other => panic!("expected Result Error, got {other:?}"),
        }
    }

    #[test]
    fn parse_rate_limit_event_falls_to_other() {
        // claude 2.x emits rate_limit_event between assistant and
        // result.
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#;
        let parsed: StreamJsonEvent = serde_json::from_str(line).unwrap();
        assert!(matches!(parsed, StreamJsonEvent::Other));
    }

    #[test]
    fn parse_unknown_event_falls_to_other_variant() {
        let line = r#"{"type":"some_future_event","payload":{"x":1}}"#;
        let parsed: StreamJsonEvent = serde_json::from_str(line).unwrap();
        assert!(matches!(parsed, StreamJsonEvent::Other));
    }

    #[test]
    fn classify_auth_error_is_config_not_transient() {
        let e = classify_claude_error("error_during_execution", "User is not logged in");
        match e {
            ExternalAgentError::Config(msg) => {
                assert!(msg.contains("not logged in"), "msg: {msg}")
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn classify_rate_limit_is_transient() {
        let e =
            classify_claude_error("error_during_execution", "rate limit exceeded for your plan");
        match e {
            ExternalAgentError::Transient(msg) => {
                assert!(msg.contains("rate limited"), "msg: {msg}")
            }
            other => panic!("expected Transient, got {other:?}"),
        }
    }

    #[test]
    fn classify_unknown_error_falls_through_to_transient() {
        let e = classify_claude_error("error_during_execution", "something broke");
        assert!(matches!(e, ExternalAgentError::Transient(_)));
    }

    #[test]
    fn into_token_usage_folds_cache_buckets() {
        let u = StreamJsonUsage {
            input_tokens: 1000,
            output_tokens: 50,
            cache_creation_input_tokens: 200,
            cache_read_input_tokens: 300,
        };
        let tu = u.into_token_usage();
        assert_eq!(tu.input_tokens, 1500);
        assert_eq!(tu.output_tokens, 50);
        assert_eq!(tu.cached_input_tokens, 300);
        assert_eq!(tu.cache_creation_input_tokens, 200);
    }

    #[test]
    fn into_token_usage_idempotent_on_zero_cache() {
        let u = StreamJsonUsage {
            input_tokens: 500,
            output_tokens: 25,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        let tu = u.into_token_usage();
        assert_eq!(tu.input_tokens, 500);
        assert_eq!(tu.cached_input_tokens, 0);
        assert_eq!(tu.cache_creation_input_tokens, 0);
    }
}
