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
//! subprocess. baybo's sandbox / sensitive_paths / approval gate do
//! NOT apply to codex's internal tool calls. `--cd <workspace_dir>`
//! pins codex's root but does not constrain its absolute-path reach.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use baybo_llm::TokenUsage;
use baybo_model::{ChatMessage, ContentBlock, ExternalAgentKind, ThinkingContent};
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use super::probe::{
    KILL_GRACE, check_binary_runs, ensure_workspace_dir, reap_after_stream_close, resolve_binary,
    spawn_stderr_tee, take_child_io,
};
use super::{
    ExternalAgent, ExternalAgentError, ExternalAgentEvent, ExternalAgentRequest,
    ExternalAgentStream, Result, mark_tool_error,
};

const AGENT_NAME: &str = "codex";

const INSTALL_HINT: &str =
    "Install codex (npm install -g @openai/codex) or configure an explicit binary path.";

/// Synthetic tool names attached to codex's `command_execution` /
/// `file_change` items when projected into baybo's `ContentBlock::ToolUse`.
/// Prefixed so a session reader can tell at a glance these are
/// codex-internal calls and were NOT routed through baybo's tool
/// registry / approval gate.
const CODEX_TOOL_SHELL: &str = "codex_shell";
const CODEX_TOOL_FILE_CHANGE: &str = "codex_file_change";

/// Bodies of the synthetic `codex_file_change` tool result. codex reports no
/// diff for an edit, so the outcome is all a reader gets.
const CODEX_FILE_CHANGE_APPLIED: &str = "applied";
const CODEX_FILE_CHANGE_FAILED: &str = "codex reported the file change did not complete";

#[derive(Debug)]
pub struct CodexCliAgent {
    binary_path: PathBuf,
    process_manager: Arc<baybo_process::ProcessManager>,
    /// `--model <NAME>` override. Empty = let codex pick from its
    /// own config (`~/.codex/`).
    model: String,
    /// Egress proxy injected into the child's env so the external CLI's
    /// own LLM calls route through it. `None` = inherit the parent env.
    proxy: Option<baybo_security::http::ProxySettings>,
}

impl CodexCliAgent {
    /// Resolve the binary + run `codex --version`. Same fail-fast
    /// shape as claude_cli.
    pub async fn probe_and_build(
        process_manager: Arc<baybo_process::ProcessManager>,
        binary_path: Option<&str>,
        proxy: Option<baybo_security::http::ProxySettings>,
    ) -> Result<Arc<Self>> {
        let resolved = resolve_binary(
            binary_path,
            ExternalAgentKind::Codex.binary_name(),
            INSTALL_HINT,
        )?;
        check_binary_runs(
            &process_manager,
            &resolved,
            ExternalAgentKind::Codex.binary_name(),
        )
        .await?;
        Ok(Arc::new(Self {
            binary_path: resolved,
            process_manager,
            model: String::new(),
            proxy,
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
    ) -> Result<baybo_process::ManagedChild> {
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
            .stderr(std::process::Stdio::piped());
        if let Some(proxy) = &self.proxy {
            for (k, v) in proxy.env_vars() {
                cmd.env(k, v);
            }
        }

        self.process_manager
            .spawn(&mut cmd, "external-agent:codex")
            .map_err(|e| {
                ExternalAgentError::Config(format!(
                    "codex: failed to spawn `{}`: {e}",
                    self.binary_path.display()
                ))
            })
    }
}

fn spawn_stream_parser(
    mut child: baybo_process::ManagedChild,
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
        let mut deadline = tokio::time::Instant::now() + timeout;

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
                        "codex: idle timeout — no output within the safety window".into()
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
                    // Any output means the subprocess is alive — reset the
                    // idle timer so only a genuinely silent run is killed.
                    deadline = tokio::time::Instant::now() + timeout;
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
                                yield Ok(ExternalAgentEvent::Intermediate(ChatMessage::assistant(
                                    vec![ContentBlock::Text(text)],
                                )));
                            }
                        }
                        ThreadEvent::ItemCompleted {
                            item: ThreadItem::Reasoning { text },
                        } => {
                            if !text.is_empty() {
                                // `Summary`, not `Text`: codex exposes only a
                                // summary of its reasoning, never the raw chain.
                                yield Ok(ExternalAgentEvent::Intermediate(ChatMessage::assistant(
                                    vec![ContentBlock::Thinking {
                                        id: None,
                                        content: vec![ThinkingContent::Summary { text }],
                                    }],
                                )));
                            }
                        }
                        ThreadEvent::ItemCompleted {
                            item:
                                ThreadItem::CommandExecution {
                                    id,
                                    command,
                                    aggregated_output,
                                    exit_code,
                                    status,
                                },
                        } => {
                            // Pair (Assistant tool_use) + (Tool
                            // tool_result) just like baybo's own agent
                            // loop. `name` is deliberately codex-
                            // qualified so a transcript reader can't
                            // confuse it with an baybo-audited tool
                            // invocation.
                            let tool_use_id = format!("codex-{id}");
                            yield Ok(ExternalAgentEvent::Intermediate(ChatMessage::assistant(
                                vec![ContentBlock::ToolUse {
                                    id: tool_use_id.clone(),
                                    name: CODEX_TOOL_SHELL.to_string(),
                                    input: json!({ "command": command }),
                                    signature: None,
                                }],
                            )));
                            // A missing exit code means codex never reported one
                            // (killed, rejected, still-running at stream end), so
                            // it counts as failure alongside a non-zero one — the
                            // transcript must not read as a clean run.
                            let failed =
                                status.failed() || exit_code.is_none_or(|code| code != 0);
                            let result = match exit_code {
                                Some(code) => format!("exit_code={code}\n{aggregated_output}"),
                                None => format!("exit_code=unknown\n{aggregated_output}"),
                            };
                            yield Ok(ExternalAgentEvent::Intermediate(
                                ChatMessage::tool_result(
                                    tool_use_id,
                                    mark_tool_error(result, failed),
                                ),
                            ));
                        }
                        ThreadEvent::ItemCompleted {
                            item: ThreadItem::FileChange { id, changes, status },
                        } => {
                            let tool_use_id = format!("codex-{id}");
                            yield Ok(ExternalAgentEvent::Intermediate(ChatMessage::assistant(
                                vec![ContentBlock::ToolUse {
                                    id: tool_use_id.clone(),
                                    name: CODEX_TOOL_FILE_CHANGE.to_string(),
                                    input: json!({ "changes": changes }),
                                    signature: None,
                                }],
                            )));
                            let failed = status.failed();
                            let result = if failed {
                                CODEX_FILE_CHANGE_FAILED
                            } else {
                                CODEX_FILE_CHANGE_APPLIED
                            };
                            yield Ok(ExternalAgentEvent::Intermediate(
                                ChatMessage::tool_result(
                                    tool_use_id,
                                    mark_tool_error(result.to_string(), failed),
                                ),
                            ));
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
/// that map onto baybo's `ContentBlock` set (text / thinking /
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
    /// codex emits its reasoning summary under `text`, NOT `summary` — the
    /// same key `AgentMessage` uses. Verified against codex-cli 0.146.1:
    /// `{"id":"item_2","type":"reasoning","text":"**Calculating …**"}`.
    /// A mismatch here is silent: `serde(default)` yields an empty string and
    /// the item is dropped, so every reasoning step vanishes from the
    /// transcript with no parse error. See the fixture test below.
    Reasoning {
        #[serde(default)]
        text: String,
    },
    CommandExecution {
        id: String,
        #[serde(default)]
        command: String,
        #[serde(default)]
        aggregated_output: String,
        #[serde(default)]
        exit_code: Option<i64>,
        #[serde(default)]
        status: ItemStatus,
    },
    FileChange {
        id: String,
        #[serde(default)]
        changes: serde_json::Value,
        #[serde(default)]
        status: ItemStatus,
    },
    #[serde(other)]
    Other,
}

/// Terminal disposition codex reports on an `item.completed` payload.
///
/// `FileChange` carries no exit code, so without this a rejected or failed
/// edit persisted the literal `"applied"` — a transcript reader asking "did
/// that edit land?" got "yes" from a run that never touched the file.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ItemStatus {
    #[default]
    Completed,
    Failed,
    /// Newer/unknown dispositions. Treated as non-success: a status codex
    /// felt the need to distinguish from `completed` is not one to record as
    /// a success in a row that is never rewritten.
    #[serde(other)]
    Other,
}

impl ItemStatus {
    fn failed(self) -> bool {
        self != Self::Completed
    }
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

    #[tokio::test]
    async fn probe_succeeds_with_working_binary() {
        let (_dir, bin) = fake_binary("codex", "codex 0.42.0");
        let agent = CodexCliAgent::probe_and_build(
            baybo_process::ProcessManager::transient(),
            Some(bin.to_str().unwrap()),
            None,
        )
        .await
        .expect("probe should succeed");
        assert_eq!(agent.kind(), ExternalAgentKind::Codex);
    }

    #[tokio::test]
    async fn probe_fails_when_binary_path_missing() {
        let err = CodexCliAgent::probe_and_build(
            baybo_process::ProcessManager::transient(),
            Some("/nonexistent/codex-binary-xyzzy"),
            None,
        )
        .await
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

    /// VERBATIM `codex exec --json` output (codex-cli 0.146.1). A hand-written
    /// fixture is worth nothing here: the previous one spelled the field
    /// `summary`, which is what the parser expected and what codex has never
    /// sent — so the test passed while every reasoning item was silently
    /// dropped in production. Replace these only by re-capturing from the CLI.
    ///
    /// Reasoning items appear only on runs long enough to warrant them; a
    /// one-line arithmetic prompt yields `reasoning_output_tokens` but no
    /// item. Capture with a task that writes a script, runs it, and checks
    /// the output.
    #[test]
    fn parse_reasoning_item_completed() {
        let line = r#"{"type":"item.completed","item":{"id":"item_2","type":"reasoning","text":"**Creating and running Armstrong script**"}}"#;
        match serde_json::from_str::<ThreadEvent>(line).unwrap() {
            ThreadEvent::ItemCompleted {
                item: ThreadItem::Reasoning { text },
            } => assert_eq!(text, "**Creating and running Armstrong script**"),
            other => panic!("expected Reasoning item, got {other:?}"),
        }
    }

    /// The shape the retired fixture asserted. Kept as a tripwire: if codex
    /// ever renames the field back, this starts parsing and the assertion
    /// fires instead of the loss going silent again.
    #[test]
    fn reasoning_item_keyed_summary_carries_no_text() {
        let line = r#"{"type":"item.completed","item":{"id":"item_4","type":"reasoning","summary":"thinking"}}"#;
        match serde_json::from_str::<ThreadEvent>(line).unwrap() {
            ThreadEvent::ItemCompleted {
                item: ThreadItem::Reasoning { text },
            } => assert!(
                text.is_empty(),
                "codex now sends reasoning under `summary`; the projection drops it"
            ),
            other => panic!("expected Reasoning item, got {other:?}"),
        }
    }

    #[test]
    fn parse_command_execution_item_completed() {
        let line = r#"{"type":"item.completed","item":{"id":"item_2","type":"command_execution","command":"/usr/bin/zsh -lc 'python3 fib.py'","aggregated_output":"0 1 1 2 3 5 8 13 21 34\n","exit_code":0,"status":"completed"}}"#;
        match serde_json::from_str::<ThreadEvent>(line).unwrap() {
            ThreadEvent::ItemCompleted {
                item:
                    ThreadItem::CommandExecution {
                        id,
                        command,
                        aggregated_output,
                        exit_code,
                        status,
                    },
            } => {
                assert_eq!(id, "item_2");
                assert_eq!(command, "/usr/bin/zsh -lc 'python3 fib.py'");
                assert_eq!(aggregated_output, "0 1 1 2 3 5 8 13 21 34\n");
                assert_eq!(exit_code, Some(0));
                assert!(!status.failed());
            }
            other => panic!("expected CommandExecution item, got {other:?}"),
        }
    }

    #[test]
    fn parse_file_change_item_completed() {
        let line = r#"{"type":"item.completed","item":{"id":"item_1","type":"file_change","changes":[{"path":"/w/fib.py","kind":"add"}],"status":"completed"}}"#;
        match serde_json::from_str::<ThreadEvent>(line).unwrap() {
            ThreadEvent::ItemCompleted {
                item:
                    ThreadItem::FileChange {
                        id,
                        changes,
                        status,
                    },
            } => {
                assert_eq!(id, "item_1");
                let arr = changes.as_array().expect("array");
                assert_eq!(arr.len(), 1);
                assert!(!status.failed());
            }
            other => panic!("expected FileChange item, got {other:?}"),
        }
    }

    #[test]
    fn a_failed_or_unknown_item_status_counts_as_failure() {
        let cases = [
            (r#""failed""#, true),
            (r#""in_progress""#, true),
            (r#""some_future_status""#, true),
            (r#""completed""#, false),
        ];
        for (raw, expect_failed) in cases {
            let line = format!(
                r#"{{"type":"item.completed","item":{{"id":"i","type":"file_change","changes":[],"status":{raw}}}}}"#
            );
            match serde_json::from_str::<ThreadEvent>(&line).unwrap() {
                ThreadEvent::ItemCompleted {
                    item: ThreadItem::FileChange { status, .. },
                } => assert_eq!(status.failed(), expect_failed, "status {raw}"),
                other => panic!("expected FileChange item, got {other:?}"),
            }
        }
    }

    /// An item with no `status` at all must not read as a clean success —
    /// `#[serde(default)]` picks `Completed`, so this pins the one case where
    /// the default is load-bearing rather than incidental.
    #[test]
    fn a_missing_item_status_defaults_to_completed() {
        let line =
            r#"{"type":"item.completed","item":{"id":"i","type":"file_change","changes":[]}}"#;
        match serde_json::from_str::<ThreadEvent>(line).unwrap() {
            ThreadEvent::ItemCompleted {
                item: ThreadItem::FileChange { status, .. },
            } => assert!(!status.failed()),
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
