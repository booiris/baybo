//! `Bash` — execute a shell command via `sh -c` inside the OS sandbox.
//!
//! Matches Claude Code's Bash tool shape: one command per call, runs in its
//! own sandboxed process. Environment variables and `cd` changes do NOT
//! persist across invocations.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ResourceAccess, Tool, ToolContext, ToolError, ToolOutput};

const MAX_OUTPUT_BYTES: usize = 64 * 1024;

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
        "Execute a shell command in a fresh `sh -c` process. Environment \
         changes and `cd` do not persist across invocations. Each of \
         stdout and stderr is truncated at 64 KiB.\n\n\
         IMPORTANT: Do NOT use Bash for tasks that have a dedicated tool:\n\
         - To read files use `Read` (not cat/head/tail)\n\
         - To write files use `Write` (not echo/cat with redirection)\n\
         - To search file names use `Glob` (not find/ls)\n\
         - To search file contents use `Grep` (not grep/rg)\n\
         `sed`, `awk`, and similar text-processing tools are fine via \
         Bash when they fit the task. Reserve Bash for system commands, \
         git operations, build/test, and terminal tasks that require \
         shell execution.\n\n\
         PATHS: Any directory or file argument inside the command (cd, ls, \
         mkdir, rm, mv, cp, find, …) MUST be an absolute path. The optional \
         `cwd` parameter MUST also be absolute when provided — relative \
         values are rejected. Always quote file paths that contain spaces \
         with double quotes (e.g. `cd \"/path with spaces/file.txt\"`).\n\n\
         SED PORTABILITY: `sed -i` is non-portable. GNU sed (Linux) accepts \
         `sed -i …`; BSD sed (macOS) requires a backup-suffix arg, e.g. \
         `sed -i '' …`. Prefer `sed -i.bak …` and remove the `*.bak` \
         afterwards if the same command needs to run on both.\n\n\
         BEFORE BROAD SCANS: Do not run `find`, `du`, recursive `ls`, or \
         similar walks against unknown directories without first checking \
         their size with a bounded probe (e.g. `ls -1 <dir> | wc -l`, or a \
         shallow `find -maxdepth 2`). Large trees can hang the process."
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
            .map(|s| {
                vec![ResourceAccess::ExecCommand {
                    command: s.to_string(),
                }]
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

        if let Some(dir) = &p.cwd
            && !dir.is_absolute()
        {
            return Err(ToolError::InvalidParams(format!(
                "Bash `cwd` must be an absolute path, got `{}`",
                dir.display()
            )));
        }

        let Some(sandbox) = ctx.sandbox.as_ref() else {
            return Err(ToolError::Execution(
                "OS sandbox unavailable: install bwrap (Linux: `apt install bubblewrap`) \
                 or sandbox-exec (macOS, ships with the system) and restart aura"
                    .into(),
            ));
        };

        let timeout = p
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(ctx.timeout);

        let command = p.command;
        let description = p.description;
        let args = vec!["-c".into(), command.clone()];
        let cwd_ref: Option<&Path> = p.cwd.as_deref();

        let out = tokio::select! {
            _ = ctx.cancellation_token.cancelled() => {
                return Err(ToolError::Execution("cancelled".into()));
            }
            res = sandbox.spawn_command(Path::new("sh"), &args, cwd_ref, None, timeout) => res?,
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
    let argv0 = leading_token(command)?;
    match argv0 {
        "grep" | "rg" | "ag" | "fgrep" | "egrep" => Some("no matches"),
        "diff" | "cmp" => Some("differences found"),
        _ => None,
    }
}

/// Extract the first real argv0 from a shell-style command string,
/// stripping `KEY=value` env-var prefixes and a small set of common
/// wrapper commands so e.g. `LD_PRELOAD=… timeout 30 grep foo bar`
/// still classifies as `grep`. Returns `None` on empty input.
///
/// Wrappers that take an argument before the inner command get one
/// extra non-flag token consumed (`timeout 30 grep …` → `grep`).
fn leading_token(command: &str) -> Option<&str> {
    /// Wrappers whose syntax is `WRAPPER [FLAGS…] INNER…` — strip the
    /// wrapper and skip flags but keep the next non-flag token.
    const PASSTHROUGH_WRAPPERS: &[&str] = &["xargs", "env", "stdbuf", "nice", "ionice"];
    /// Wrappers whose syntax is `WRAPPER [FLAGS…] ARG INNER…` — also
    /// consume one extra non-flag token (the wrapper's own argument)
    /// before the inner command.
    const ARG_WRAPPERS: &[&str] = &["timeout"];

    let mut tokens = command.split_whitespace();
    while let Some(raw) = tokens.next() {
        let bare = raw.trim_start_matches('\\');
        if bare.contains('=') && !bare.starts_with('=') {
            continue;
        }
        if bare.starts_with('-') {
            continue;
        }
        let stripped = strip_path(bare);
        if PASSTHROUGH_WRAPPERS.contains(&stripped) {
            continue;
        }
        if ARG_WRAPPERS.contains(&stripped) {
            // Discard the wrapper's own arg (e.g. `30` in `timeout 30 grep`),
            // skipping any flags that precede it.
            for arg in tokens.by_ref() {
                if !arg.starts_with('-') {
                    break;
                }
            }
            continue;
        }
        return Some(stripped);
    }
    None
}

fn strip_path(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
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
    use crate::test_support::FakeExecSandbox;
    use aura_model::{ChannelType, User};
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

    fn fake_with_response(
        out: SandboxedOutput,
    ) -> (Arc<FakeExecSandbox>, Arc<dyn crate::ExecSandbox>) {
        let fake = Arc::new(FakeExecSandbox::new());
        fake.set_response(out);
        let dyn_handle: Arc<dyn crate::ExecSandbox> = fake.clone();
        (fake, dyn_handle)
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
            .execute(
                json!({ "command": "ls /nonexistent" }),
                &ctx_with(Some(sandbox)),
            )
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
                json!({ "command": "ls", "description": "List files in current directory" }),
                &ctx_with(Some(sandbox)),
            )
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else { panic!() };
        assert_eq!(v["description"], "List files in current directory");
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
    fn leading_token_strips_env_and_wrappers() {
        assert_eq!(leading_token("grep foo bar"), Some("grep"));
        assert_eq!(
            leading_token("LD_PRELOAD=x timeout 30 grep foo"),
            Some("grep")
        );
        assert_eq!(leading_token("/usr/bin/grep foo"), Some("grep"));
        assert_eq!(leading_token("env FOO=bar rg pat"), Some("rg"));
        assert_eq!(leading_token(""), None);
    }
}
