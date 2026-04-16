//! `Bash` — execute a shell command via `sh -c`.
//!
//! Matches Claude Code's Bash tool shape: one command per call, runs in its
//! own process. Environment variables and `cd` changes do NOT persist across
//! invocations (each call is a fresh `sh -c`).

use std::path::PathBuf;
use std::process::Stdio;
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
         stdout/stderr output is truncated at 64 KiB."
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

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(&p.command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(dir) = &p.cwd {
            cmd.current_dir(dir);
        }

        let timeout = p
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(ctx.timeout);

        let run = async {
            let child = cmd
                .spawn()
                .map_err(|e| ToolError::Execution(format!("spawn: {e}")))?;
            let out = child
                .wait_with_output()
                .await
                .map_err(|e| ToolError::Execution(format!("wait: {e}")))?;
            Ok::<_, ToolError>(out)
        };

        let out = tokio::select! {
            _ = ctx.cancellation_token.cancelled() => {
                return Err(ToolError::Execution("cancelled".into()));
            }
            res = tokio::time::timeout(timeout, run) => {
                res.map_err(|_| ToolError::Timeout(format!("Bash exceeded {timeout:?}")))??
            }
        };

        let exit = out.status.code().unwrap_or(-1);
        let stdout = truncate_utf8(&out.stdout, MAX_OUTPUT_BYTES);
        let stderr = truncate_utf8(&out.stderr, MAX_OUTPUT_BYTES);

        Ok(ToolOutput::Json(json!({
            "exit_code": exit,
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
    use aura_model::{ChannelType, User};
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        ToolContext {
            session_id: "t".into(),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::Tui,
            },
            timeout: Duration::from_secs(5),
            cancellation_token: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn captures_stdout_and_exit() {
        let out = BashTool
            .execute(json!({ "command": "echo hello" }), &ctx())
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else {
            panic!();
        };
        assert_eq!(v["exit_code"], 0);
        assert!(v["stdout"].as_str().unwrap().contains("hello"));
    }

    #[tokio::test]
    async fn reports_non_zero_exit() {
        let out = BashTool
            .execute(json!({ "command": "exit 7" }), &ctx())
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else {
            panic!();
        };
        assert_eq!(v["exit_code"], 7);
    }

    #[tokio::test]
    async fn times_out() {
        let err = BashTool
            .execute(json!({ "command": "sleep 5", "timeout_ms": 50 }), &ctx())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Timeout(_)));
    }
}
