//! `codex` external agent — drives OpenAI's `codex` CLI in non-
//! interactive `exec --json` mode.
//!
//! Wire protocol: see codex-rs/exec/src/exec_events.rs in the OpenAI
//! codex repo. Notable differences from claude_cli:
//!   - prompt is positional argv, not stdin
//!   - session-id event is `thread.started { thread_id }`, not
//!     `system/init { session_id }`
//!   - assistant text arrives as a single `item.completed
//!     { type:"agent_message", text:"..." }` per turn (no per-token
//!     deltas)
//!   - Final usage / completion lives on `turn.completed { usage }`
//!   - resume is a SUBCOMMAND (`codex exec resume <id> "<prompt>"`),
//!     not a flag
//!   - `cached_input_tokens` is a SUBSET of `input_tokens` (NOT
//!     additive like claude's cache_creation/cache_read split)
//!
//! Security: `--dangerously-bypass-approvals-and-sandbox` is hardcoded
//! — codex's interactive permission prompts can't reach a non-TTY
//! subprocess. aura's sandbox / sensitive_paths / approval gate do
//! NOT apply to codex's internal tool calls. `--cd <workspace_dir>`
//! pins codex's root but does not constrain its absolute-path reach.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aura_llm::TokenUsage;
use aura_model::{ChatMessage, ContentBlock, ExternalAgentKind, Role, ThinkingContent};
use serde::Deserialize;
use serde_json::json;
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

const AGENT_NAME: &str = "codex";

const INSTALL_HINT: &str =
    "Install codex (npm install -g @openai/codex) or configure an explicit binary path.";

/// Synthetic tool names attached to codex's `command_execution` /
/// `file_change` items when projected into aura's `ContentBlock::ToolUse`.
/// Prefixed so a session reader can tell at a glance these are
/// codex-internal calls and were NOT routed through aura's tool
/// registry / approval gate.
const CODEX_TOOL_SHELL: &str = "codex_shell";
const CODEX_TOOL_FILE_CHANGE: &str = "codex_file_change";

#[derive(Debug)]
pub struct CodexCliAgent {
    binary_path: PathBuf,
    /// `--model <NAME>` override. Empty = let codex pick from its
    /// own config (`~/.codex/`).
    model: String,
}

impl CodexCliAgent {
    /// Resolve the binary + run `codex --version`. Same fail-fast
    /// shape as claude_cli.
    pub fn probe_and_build(binary_path: Option<&str>) -> Result<Arc<Self>> {
        let resolved = resolve_binary(
            binary_path,
            ExternalAgentKind::Codex.binary_name(),
            INSTALL_HINT,
        )?;
        check_binary_runs(&resolved, ExternalAgentKind::Codex.binary_name())?;
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
impl ExternalAgent for CodexCliAgent {
    fn kind(&self) -> ExternalAgentKind {
        ExternalAgentKind::Codex
    }

    async fn run(&self, request: ExternalAgentRequest) -> Result<ExternalAgentStream> {
        ensure_workspace_dir(&request.workspace_dir, AGENT_NAME).await?;

        let child = self
            .spawn_codex(
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

impl CodexCliAgent {
    async fn spawn_codex(
        &self,
        workspace_dir: &Path,
        resume_id: Option<&str>,
        prompt: &str,
    ) -> Result<Child> {
        let mut cmd = Command::new(&self.binary_path);
        // Global flags must come BEFORE the `resume` subcommand.
        cmd.arg("exec")
            .arg("--json")
            .arg("--skip-git-repo-check")
            .arg("--dangerously-bypass-approvals-and-sandbox")
            .arg("--cd")
            .arg(workspace_dir);
        if !self.model.is_empty() {
            cmd.arg("--model").arg(&self.model);
        }
        if let Some(id) = resume_id {
            cmd.arg("resume").arg(id);
        }
        // `--` separator so a prompt starting with `-` isn't parsed as
        // a flag. Safe in both fresh and resume invocations.
        cmd.arg("--").arg(prompt);

        cmd.current_dir(workspace_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        cmd.spawn().map_err(|e| {
            ExternalAgentError::Config(format!(
                "codex: failed to spawn `{}`: {e}",
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
        let mut final_text = String::new();
        let mut buffered_usage: Option<TokenUsage> = None;
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    let _ = child.start_kill();
                    let _ = tokio::time::timeout(KILL_GRACE, child.wait()).await;
                    yield Err(ExternalAgentError::Transient("codex: cancelled by parent".into()));
                    return;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let _ = child.start_kill();
                    let _ = tokio::time::timeout(KILL_GRACE, child.wait()).await;
                    yield Err(ExternalAgentError::Transient(
                        "codex: exceeded declared timeout".into()
                    ));
                    return;
                }
                line_result = reader.next_line() => {
                    let line = match line_result {
                        Ok(Some(l)) => l,
                        Ok(None) => break,
                        Err(e) => {
                            yield Err(ExternalAgentError::Transient(format!(
                                "codex: read stdout: {e}"
                            )));
                            return;
                        }
                    };
                    if line.is_empty() {
                        continue;
                    }
                    let Ok(event) = serde_json::from_str::<ThreadEvent>(&line) else {
                        // Forward-compat: unknown event shape, skip.
                        continue;
                    };
                    match event {
                        ThreadEvent::ThreadStarted { thread_id }
                            if is_fresh_session && !session_persisted =>
                        {
                            session_persisted = true;
                            yield Ok(ExternalAgentEvent::ResumeKey(thread_id));
                        }
                        ThreadEvent::ItemCompleted {
                            item: ThreadItem::AgentMessage { text },
                        } => {
                            if !text.is_empty() {
                                final_text.clone_from(&text);
                                yield Ok(ExternalAgentEvent::TextDelta(text.clone()));
                                yield Ok(ExternalAgentEvent::Intermediate(ChatMessage {
                                    role: Role::Assistant,
                                    content: vec![ContentBlock::Text(text)],
                                    from_user: false,
                                }));
                            }
                        }
                        ThreadEvent::ItemCompleted {
                            item: ThreadItem::Reasoning { summary },
                        } => {
                            if !summary.is_empty() {
                                yield Ok(ExternalAgentEvent::Intermediate(ChatMessage {
                                    role: Role::Assistant,
                                    content: vec![ContentBlock::Thinking {
                                        id: None,
                                        content: vec![ThinkingContent::Summary { text: summary }],
                                    }],
                                    from_user: false,
                                }));
                            }
                        }
                        ThreadEvent::ItemCompleted {
                            item:
                                ThreadItem::CommandExecution {
                                    id,
                                    command,
                                    aggregated_output,
                                    exit_code,
                                },
                        } => {
                            // Pair (Assistant tool_use) + (Tool
                            // tool_result) just like aura's own agent
                            // loop. `name` is deliberately codex-
                            // qualified so a transcript reader can't
                            // confuse it with an aura-audited tool
                            // invocation.
                            let tool_use_id = format!("codex-{id}");
                            yield Ok(ExternalAgentEvent::Intermediate(ChatMessage {
                                role: Role::Assistant,
                                content: vec![ContentBlock::ToolUse {
                                    id: tool_use_id.clone(),
                                    name: CODEX_TOOL_SHELL.to_string(),
                                    input: json!({ "command": command }),
                                    signature: None,
                                }],
                                from_user: false,
                            }));
                            let result = match exit_code {
                                Some(code) => format!("exit_code={code}\n{aggregated_output}"),
                                None => aggregated_output,
                            };
                            yield Ok(ExternalAgentEvent::Intermediate(ChatMessage {
                                role: Role::Tool,
                                content: vec![ContentBlock::ToolResult {
                                    tool_use_id,
                                    content: result,
                                }],
                                from_user: false,
                            }));
                        }
                        ThreadEvent::ItemCompleted {
                            item: ThreadItem::FileChange { id, changes },
                        } => {
                            let tool_use_id = format!("codex-{id}");
                            yield Ok(ExternalAgentEvent::Intermediate(ChatMessage {
                                role: Role::Assistant,
                                content: vec![ContentBlock::ToolUse {
                                    id: tool_use_id.clone(),
                                    name: CODEX_TOOL_FILE_CHANGE.to_string(),
                                    input: json!({ "changes": changes }),
                                    signature: None,
                                }],
                                from_user: false,
                            }));
                            yield Ok(ExternalAgentEvent::Intermediate(ChatMessage {
                                role: Role::Tool,
                                content: vec![ContentBlock::ToolResult {
                                    tool_use_id,
                                    content: "applied".to_string(),
                                }],
                                from_user: false,
                            }));
                        }
                        ThreadEvent::TurnCompleted { usage } => {
                            buffered_usage = usage.map(CodexUsage::into_token_usage);
                            // Hold FinalContent / Usage until after
                            // stdout EOF + reap so a consumer that
                            // breaks on FinalContent doesn't drop the
                            // stream while codex is still flushing.
                        }
                        ThreadEvent::TurnFailed { error } => {
                            let stderr_snapshot = stderr_buf.lock().clone();
                            let detail = if error.message.is_empty() {
                                stderr_snapshot.trim().to_string()
                            } else {
                                error.message.clone()
                            };
                            yield Err(classify_codex_error(&detail));
                            return;
                        }
                        ThreadEvent::Error { message } => {
                            yield Err(classify_codex_error(&message));
                            return;
                        }
                        // ItemStarted / ItemUpdated / TurnStarted /
                        // unknown variants: ignore.
                        _ => {}
                    }
                }
            }
        }
        if let Err(e) = reap_after_stream_close(&mut child, AGENT_NAME, &stderr_buf).await {
            yield Err(e);
            return;
        }
        if let Some(u) = buffered_usage {
            yield Ok(ExternalAgentEvent::Usage(u));
        }
        let blocks = if final_text.is_empty() {
            Vec::new()
        } else {
            vec![ContentBlock::Text(final_text)]
        };
        yield Ok(ExternalAgentEvent::FinalContent(blocks));
    };

    Ok(Box::pin(stream))
}

fn classify_codex_error(detail: &str) -> ExternalAgentError {
    let lowered = detail.to_ascii_lowercase();
    let auth_hit = lowered.contains("logged out")
        || lowered.contains("not logged in")
        || lowered.contains("authenticate")
        || lowered.contains("unauthorized");
    if auth_hit {
        return ExternalAgentError::Config(format!(
            "codex: not logged in — run `codex login`. Detail: {detail}"
        ));
    }
    let rate_hit = lowered.contains("rate limit")
        || lowered.contains("quota")
        || lowered.contains("usage limit")
        || lowered.contains("too many requests");
    if rate_hit {
        return ExternalAgentError::Transient(format!("codex: rate limited: {detail}"));
    }
    ExternalAgentError::Transient(format!("codex: {detail}"))
}

// ---------------------------------------------------------------------------
// JSONL event shapes (codex-rs/exec/src/exec_events.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ThreadEvent {
    #[serde(rename = "thread.started")]
    ThreadStarted { thread_id: String },
    #[serde(rename = "turn.started")]
    TurnStarted,
    #[serde(rename = "turn.completed")]
    TurnCompleted {
        #[serde(default)]
        usage: Option<CodexUsage>,
    },
    #[serde(rename = "turn.failed")]
    TurnFailed { error: CodexError },
    #[serde(rename = "item.started")]
    ItemStarted {
        #[allow(dead_code)]
        item: serde_json::Value,
    },
    #[serde(rename = "item.updated")]
    ItemUpdated {
        #[allow(dead_code)]
        item: serde_json::Value,
    },
    #[serde(rename = "item.completed")]
    ItemCompleted { item: ThreadItem },
    #[serde(rename = "error")]
    Error { message: String },
    #[serde(other)]
    Other,
}

/// `item.completed` payloads codex emits. We surface the four kinds
/// that map onto aura's `ContentBlock` set (text / thinking /
/// tool_use+tool_result for shell + file edits). Newer codex item
/// kinds fall through `Other`.
///
/// Codex assigns synthetic `item_<N>` ids; the spawn router pairs the
/// tool_use / tool_result halves of a command_execution or
/// file_change via these ids so a downstream session reader can see
/// "what was run" right next to "what it returned".
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ThreadItem {
    AgentMessage {
        #[serde(default)]
        text: String,
    },
    Reasoning {
        #[serde(default)]
        summary: String,
    },
    CommandExecution {
        id: String,
        #[serde(default)]
        command: String,
        #[serde(default)]
        aggregated_output: String,
        #[serde(default)]
        exit_code: Option<i64>,
    },
    FileChange {
        id: String,
        #[serde(default)]
        changes: serde_json::Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct CodexError {
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct CodexUsage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(default)]
    cached_input_tokens: i64,
    #[serde(default)]
    #[allow(dead_code)]
    reasoning_output_tokens: i64,
}

impl CodexUsage {
    fn into_token_usage(self) -> TokenUsage {
        // codex reports cached_input_tokens as a SUBSET of
        // input_tokens (not additive like claude). Pass through
        // directly; downstream cost code handles the
        // `cached_input_tokens` field as "of the input tokens, this
        // many were served from cache", which matches codex's
        // contract.
        TokenUsage {
            input_tokens: clamp_to_usize(self.input_tokens),
            output_tokens: clamp_to_usize(self.output_tokens),
            cached_input_tokens: clamp_to_usize(self.cached_input_tokens),
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
        let (_dir, bin) = fake_binary("codex", "codex 0.42.0");
        let agent = CodexCliAgent::probe_and_build(Some(bin.to_str().unwrap()))
            .expect("probe should succeed");
        assert_eq!(agent.kind(), ExternalAgentKind::Codex);
    }

    #[test]
    fn probe_fails_when_binary_path_missing() {
        let err = CodexCliAgent::probe_and_build(Some("/nonexistent/codex-binary-xyzzy"))
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
    fn parse_thread_started_event() {
        let line = r#"{"type":"thread.started","thread_id":"abc-uuid-123"}"#;
        match serde_json::from_str::<ThreadEvent>(line).unwrap() {
            ThreadEvent::ThreadStarted { thread_id } => assert_eq!(thread_id, "abc-uuid-123"),
            other => panic!("expected ThreadStarted, got {other:?}"),
        }
    }

    #[test]
    fn parse_agent_message_item_completed() {
        let line = r#"{"type":"item.completed","item":{"id":"item_5","type":"agent_message","text":"hello world"}}"#;
        match serde_json::from_str::<ThreadEvent>(line).unwrap() {
            ThreadEvent::ItemCompleted {
                item: ThreadItem::AgentMessage { text },
            } => assert_eq!(text, "hello world"),
            other => panic!("expected AgentMessage item, got {other:?}"),
        }
    }

    #[test]
    fn parse_reasoning_item_completed() {
        let line = r#"{"type":"item.completed","item":{"id":"item_4","type":"reasoning","summary":"thinking"}}"#;
        match serde_json::from_str::<ThreadEvent>(line).unwrap() {
            ThreadEvent::ItemCompleted {
                item: ThreadItem::Reasoning { summary },
            } => assert_eq!(summary, "thinking"),
            other => panic!("expected Reasoning item, got {other:?}"),
        }
    }

    #[test]
    fn parse_command_execution_item_completed() {
        let line = r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"ls","aggregated_output":"foo\n","exit_code":0,"status":"completed"}}"#;
        match serde_json::from_str::<ThreadEvent>(line).unwrap() {
            ThreadEvent::ItemCompleted {
                item:
                    ThreadItem::CommandExecution {
                        id,
                        command,
                        aggregated_output,
                        exit_code,
                    },
            } => {
                assert_eq!(id, "item_1");
                assert_eq!(command, "ls");
                assert_eq!(aggregated_output, "foo\n");
                assert_eq!(exit_code, Some(0));
            }
            other => panic!("expected CommandExecution item, got {other:?}"),
        }
    }

    #[test]
    fn parse_file_change_item_completed() {
        let line = r#"{"type":"item.completed","item":{"id":"item_3","type":"file_change","changes":[{"path":"/x","kind":"add"}],"status":"completed"}}"#;
        match serde_json::from_str::<ThreadEvent>(line).unwrap() {
            ThreadEvent::ItemCompleted {
                item: ThreadItem::FileChange { id, changes },
            } => {
                assert_eq!(id, "item_3");
                let arr = changes.as_array().expect("array");
                assert_eq!(arr.len(), 1);
            }
            other => panic!("expected FileChange item, got {other:?}"),
        }
    }

    #[test]
    fn parse_turn_completed_with_usage() {
        let line = r#"{"type":"turn.completed","usage":{"input_tokens":1000,"output_tokens":50,"cached_input_tokens":300,"reasoning_output_tokens":25}}"#;
        match serde_json::from_str::<ThreadEvent>(line).unwrap() {
            ThreadEvent::TurnCompleted { usage: Some(u) } => {
                let tu = u.into_token_usage();
                // codex: cached is subset of input — pass through.
                assert_eq!(tu.input_tokens, 1000);
                assert_eq!(tu.output_tokens, 50);
                assert_eq!(tu.cached_input_tokens, 300);
                assert_eq!(tu.cache_creation_input_tokens, 0);
            }
            other => panic!("expected TurnCompleted with usage, got {other:?}"),
        }
    }

    #[test]
    fn parse_turn_failed() {
        let line = r#"{"type":"turn.failed","error":{"message":"something went wrong"}}"#;
        match serde_json::from_str::<ThreadEvent>(line).unwrap() {
            ThreadEvent::TurnFailed { error } => {
                assert_eq!(error.message, "something went wrong");
            }
            other => panic!("expected TurnFailed, got {other:?}"),
        }
    }

    #[test]
    fn parse_top_level_error() {
        let line = r#"{"type":"error","message":"stream broke"}"#;
        match serde_json::from_str::<ThreadEvent>(line).unwrap() {
            ThreadEvent::Error { message } => assert_eq!(message, "stream broke"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn parse_unknown_event_falls_to_other() {
        let line = r#"{"type":"some.future.event","payload":{}}"#;
        let parsed: ThreadEvent = serde_json::from_str(line).unwrap();
        assert!(matches!(parsed, ThreadEvent::Other));
    }

    #[test]
    fn classify_auth_error_is_config() {
        let e = classify_codex_error("Unauthorized: please run codex login");
        match e {
            ExternalAgentError::Config(msg) => assert!(msg.contains("not logged in"), "msg: {msg}"),
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn classify_rate_limit_is_transient() {
        let e = classify_codex_error("rate limit exceeded");
        match e {
            ExternalAgentError::Transient(msg) => assert!(msg.contains("rate limited"), "msg: {msg}"),
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
