//! Shared helpers for shelling out to the `rg` (ripgrep) binary, used by
//! both `Grep` (content search) and `Glob` (filename matching). Owns the
//! spawn / streamed-read / cancellation boilerplate and the flags common
//! to every invocation; each caller appends its own mode-specific args
//! and consumes stdout through its own [`StdoutSink`].

use std::ops::ControlFlow;
use std::process::Stdio;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::{ToolContext, ToolError};

/// Cap on raw `rg` stdout [`CappedBuffer`] holds in-process. Past this we
/// stop reading and let `rg` exit on EPIPE. Callers that stream
/// newest-first keep the most relevant rows even when the tail is dropped.
pub(super) const MAX_STDOUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_STDERR_BYTES: u64 = 64 * 1024;
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// Consumes `rg` stdout as it arrives, so a caller can filter while it
/// reads and bound what it keeps rather than what rg prints.
pub(super) trait StdoutSink: Send + 'static {
    /// Take the next chunk. `Break` stops reading; rg then exits on EPIPE.
    fn feed(&mut self, chunk: &[u8]) -> ControlFlow<()>;
}

/// Keeps the first [`MAX_STDOUT_BYTES`] of stdout verbatim.
#[derive(Default)]
pub(super) struct CappedBuffer {
    pub(super) bytes: Vec<u8>,
}

impl StdoutSink for CappedBuffer {
    fn feed(&mut self, chunk: &[u8]) -> ControlFlow<()> {
        let room = MAX_STDOUT_BYTES - self.bytes.len();
        self.bytes
            .extend_from_slice(&chunk[..chunk.len().min(room)]);
        if chunk.len() >= room {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }
}

/// Result of an `rg` run. `code` is rg's exit status (0 = matches,
/// 1 = no matches, 2+ = error, `None` = killed by a signal) and is left
/// for the caller to interpret, since `Grep` and `Glob` treat traversal
/// errors differently.
pub(super) struct Captured<S> {
    pub(super) code: Option<i32>,
    pub(super) stdout: S,
    /// The sink stopped reading before rg finished writing.
    pub(super) stdout_closed_early: bool,
    pub(super) stderr: Vec<u8>,
}

impl<S> Captured<S> {
    /// Whether the exit status means rg's output can't be trusted as a
    /// finished run. A signal death after we closed the pipe is just rg
    /// reacting to that — the sink already knows why it stopped.
    pub(super) fn failed(&self) -> bool {
        match self.code {
            Some(code) => code >= 2,
            None => !self.stdout_closed_early,
        }
    }

    /// Render rg's stderr into a tool error for the [`Self::failed`] case.
    pub(super) fn into_error(self) -> ToolError {
        let stderr = String::from_utf8_lossy(&self.stderr);
        let status = match self.code {
            Some(code) => format!("exited with code {code}"),
            None => "was terminated by a signal".to_string(),
        };
        ToolError::Execution(format!("rg {status}: {}", stderr.trim()))
    }
}

/// Spawn `rg` with the common flags plus whatever `configure` appends,
/// stream its stdout into `sink`, wait for it (aborting on cancellation),
/// and return the result. Spawn / wait / pipe-read failures surface as
/// `Err`; a non-zero exit is reported via [`Captured::code`].
pub(super) async fn capture<S: StdoutSink>(
    process_manager: &Arc<baybo_process::ProcessManager>,
    ctx: &ToolContext,
    sink: S,
    configure: impl FnOnce(&mut Command),
) -> crate::Result<Captured<S>> {
    let mut cmd = Command::new("rg");
    cmd.arg("--no-config")
        .arg("--no-messages")
        .arg("--color=never");
    configure(&mut cmd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = process_manager
        .spawn(&mut cmd, "ripgrep")
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => ToolError::Execution(
                "ripgrep (`rg`) not found on PATH; install it (e.g. `apt install ripgrep`, \
             `brew install ripgrep`, `pacman -S ripgrep`)"
                    .into(),
            ),
            _ => ToolError::Execution(format!("spawn rg: {e}")),
        })?;

    let stdout_pipe = child
        .take_stdout()
        .ok_or_else(|| ToolError::Execution("rg stdout pipe missing".into()))?;
    let stderr_pipe = child
        .take_stderr()
        .ok_or_else(|| ToolError::Execution("rg stderr pipe missing".into()))?;

    let stdout_task = tokio::spawn(drain(stdout_pipe, sink));
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        stderr_pipe
            .take(MAX_STDERR_BYTES)
            .read_to_end(&mut buf)
            .await
            .map(|_| buf)
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
    let (stdout, stdout_closed_early) = join_reader(stdout_task, "stdout").await?;
    let stderr = join_reader(stderr_task, "stderr").await?;

    Ok(Captured {
        code: exit_status.code(),
        stdout,
        stdout_closed_early,
        stderr,
    })
}

/// Feed `pipe` into `sink` until EOF or until the sink breaks; reports
/// whether it broke. Dropping the pipe on return is what closes it.
async fn drain<S: StdoutSink>(
    mut pipe: impl AsyncRead + Unpin,
    mut sink: S,
) -> std::io::Result<(S, bool)> {
    let mut chunk = vec![0; READ_CHUNK_BYTES];
    loop {
        let n = pipe.read(&mut chunk).await?;
        if n == 0 {
            return Ok((sink, false));
        }
        if sink.feed(&chunk[..n]).is_break() {
            return Ok((sink, true));
        }
    }
}

async fn join_reader<T>(
    task: tokio::task::JoinHandle<std::io::Result<T>>,
    stream: &str,
) -> crate::Result<T> {
    task.await
        .map_err(|e| ToolError::Execution(format!("rg {stream} reader failed: {e}")))?
        .map_err(|e| ToolError::Execution(format!("read rg {stream}: {e}")))
}

/// Decode `path1\0path2\0…` records from `rg … --null`. Non-UTF-8 paths
/// are dropped — we can't safely round-trip them through the JSON tool
/// result anyway.
pub(super) fn iter_null_paths(stdout: &[u8]) -> impl Iterator<Item = String> + '_ {
    stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| std::str::from_utf8(s).ok().map(String::from))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_capped_buffer_breaks_only_once_it_is_full() {
        let mut sink = CappedBuffer::default();
        assert!(sink.feed(&[1; MAX_STDOUT_BYTES - 1]).is_continue());
        assert!(sink.feed(&[2; 3]).is_break());
        assert_eq!(sink.bytes.len(), MAX_STDOUT_BYTES);
        assert_eq!(sink.bytes.last(), Some(&2));
    }

    #[tokio::test]
    async fn drain_reports_whether_the_sink_stopped_it() {
        let (sink, early) = drain(&b"abc"[..], CappedBuffer::default()).await.unwrap();
        assert_eq!((sink.bytes.as_slice(), early), (&b"abc"[..], false));

        let big = vec![0u8; MAX_STDOUT_BYTES + 1];
        let (sink, early) = drain(big.as_slice(), CappedBuffer::default())
            .await
            .unwrap();
        assert_eq!((sink.bytes.len(), early), (MAX_STDOUT_BYTES, true));
    }

    fn captured(code: Option<i32>, stdout_closed_early: bool) -> Captured<()> {
        Captured {
            code,
            stdout: (),
            stdout_closed_early,
            stderr: Vec::new(),
        }
    }

    #[test]
    fn a_signal_death_fails_unless_we_closed_the_pipe_first() {
        assert!(!captured(Some(0), false).failed());
        assert!(!captured(Some(1), false).failed());
        assert!(captured(Some(2), false).failed());
        assert!(captured(None, false).failed());
        assert!(!captured(None, true).failed());
        assert!(
            matches!(captured(None, false).into_error(), ToolError::Execution(ref m) if m.contains("signal"))
        );
    }

    /// Stops after `limit` bytes, to close the pipe on a live rg.
    struct StopAfter(usize);

    impl StdoutSink for StopAfter {
        fn feed(&mut self, chunk: &[u8]) -> ControlFlow<()> {
            self.0 = self.0.saturating_sub(chunk.len());
            if self.0 == 0 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        }
    }

    #[tokio::test]
    async fn closing_the_pipe_early_is_not_an_rg_failure() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..2000 {
            tokio::fs::write(dir.path().join(format!("f{i}.txt")), "x")
                .await
                .unwrap();
        }
        let pm = baybo_process::ProcessManager::transient();
        let ctx = ToolContext::for_test();
        let cap = capture(&pm, &ctx, StopAfter(16), |cmd| {
            cmd.arg("--files").arg("--null").arg(dir.path());
        })
        .await
        .unwrap();
        assert!(cap.stdout_closed_early);
        assert!(!cap.failed());
    }
}
