//! Shared helpers for shelling out to the `rg` (ripgrep) binary, used by
//! both `Grep` (content search) and `Glob` (filename matching). Owns the
//! spawn / bounded-read / cancellation boilerplate and the flags common
//! to every invocation; each caller appends its own mode-specific args
//! and parses the raw stdout itself.

use std::process::Stdio;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::{ToolContext, ToolError};

/// Cap on raw `rg` stdout we buffer in-process. Past this we stop reading
/// and let `rg` exit on EPIPE. Callers that stream newest-first keep the
/// most relevant rows even when the tail is dropped.
pub(super) const MAX_STDOUT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_STDERR_BYTES: u64 = 64 * 1024;

/// Result of an `rg` run. `code` is rg's exit status (0 = matches,
/// 1 = no matches, 2+ = error) and is left for the caller to interpret,
/// since `Grep` and `Glob` treat traversal errors differently.
pub(super) struct Captured {
    pub(super) code: i32,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

impl Captured {
    /// Render rg's stderr into a tool error for the `code >= 2` case.
    pub(super) fn into_error(self) -> ToolError {
        let stderr = String::from_utf8_lossy(&self.stderr);
        ToolError::Execution(format!(
            "rg exited with code {}: {}",
            self.code,
            stderr.trim()
        ))
    }
}

/// Spawn `rg` with the common flags plus whatever `configure` appends,
/// wait for it (aborting on cancellation), and return the captured
/// output. Only spawn / wait / pipe failures surface as `Err`; a
/// non-zero exit is reported via [`Captured::code`].
pub(super) async fn capture(
    ctx: &ToolContext,
    max_stdout: u64,
    configure: impl FnOnce(&mut Command),
) -> crate::Result<Captured> {
    let mut cmd = Command::new("rg");
    cmd.arg("--no-config")
        .arg("--no-messages")
        .arg("--color=never");
    configure(&mut cmd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ToolError::Execution(
            "ripgrep (`rg`) not found on PATH; install it (e.g. `apt install ripgrep`, \
             `brew install ripgrep`, `pacman -S ripgrep`)"
                .into(),
        ),
        _ => ToolError::Execution(format!("spawn rg: {e}")),
    })?;

    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| ToolError::Execution("rg stdout pipe missing".into()))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| ToolError::Execution("rg stderr pipe missing".into()))?;

    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut limited = stdout_pipe.take(max_stdout);
        let _ = limited.read_to_end(&mut buf).await;
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut limited = stderr_pipe.take(MAX_STDERR_BYTES);
        let _ = limited.read_to_end(&mut buf).await;
        buf
    });

    let exit = tokio::select! {
        _ = ctx.cancellation_token.cancelled() => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(ToolError::Execution("cancelled".into()));
        }
        wait = child.wait() => wait,
    };
    let exit_status = exit.map_err(|e| ToolError::Execution(format!("rg wait: {e}")))?;
    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();

    Ok(Captured {
        code: exit_status.code().unwrap_or(-1),
        stdout,
        stderr,
    })
}

/// Decode `path1\0path2\0…` records from `rg … --null`. Non-UTF-8 paths
/// are dropped — we can't safely round-trip them through the JSON tool
/// result anyway, and a sensitive non-UTF-8 path would otherwise have no
/// way of being matched against `is_sensitive_path`.
pub(super) fn iter_null_paths(stdout: &[u8]) -> impl Iterator<Item = String> + '_ {
    stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| std::str::from_utf8(s).ok().map(String::from))
}
