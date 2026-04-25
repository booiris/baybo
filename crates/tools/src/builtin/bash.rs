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
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        "Execute a shell command in a fresh `sh -c` process. Environment \
         changes and `cd` do not persist across invocations. Combined \
         stdout/stderr output is truncated at 64 KiB.\n\n\
         IMPORTANT: Do NOT use Bash for tasks that have a dedicated tool:\n\
         - To read files use `Read` (not cat/head/tail/sed)\n\
         - To write files use `Write` (not echo/cat with redirection)\n\
         - To edit files use `Edit` (not sed/awk)\n\
         - To search file names use `Glob` (not find/ls)\n\
         - To search file contents use `Grep` (not grep/rg)\n\
         Reserve Bash for system commands, git operations, build/test, \
         and terminal tasks that require shell execution.\n\n\
         PATHS: Any directory or file argument inside the command (cd, ls, \
         mkdir, rm, mv, cp, find, …) MUST be an absolute path. The optional \
         `cwd` parameter MUST also be absolute when provided — relative \
         values are rejected.\n\n\
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
                "cwd":        { "type": "string", "description": "Working directory for the command" }
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

        let args = vec!["-c".into(), p.command];
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

        Ok(ToolOutput::Json(json!({
            "exit_code": out.exit_code,
            "stdout": stdout,
            "stderr": stderr,
        })))
    }
}

fn truncate_utf8(bytes: &[u8], max: usize) -> String {
    if bytes.len() <= max {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut cut = max;
    while cut > 0 && (bytes[cut] & 0b1100_0000) == 0b1000_0000 {
        cut -= 1;
    }
    let mut s = String::from_utf8_lossy(&bytes[..cut]).into_owned();
    s.push_str("\n… [truncated]");
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
}
