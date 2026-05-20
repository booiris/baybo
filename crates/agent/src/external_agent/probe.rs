//! Shared subprocess helpers for external-agent impls — boot-time
//! probe (binary resolution + `<bin> --version` check) and run-time
//! plumbing (stderr tee buffer, workspace dir prep, child reap).
//! The per-agent stream parsers (claude_cli, codex_cli) handle the
//! protocol-specific event matching; this module owns the parts
//! that don't vary.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout};

use super::{ExternalAgentError, Result};

/// Grace window between `start_kill()` and the forced wait when an
/// external agent is cancelled / times out.
pub(crate) const KILL_GRACE: Duration = Duration::from_secs(3);

/// Cap on the in-memory stderr capture used to fold a useful message
/// into the error returned on non-zero exit. Beyond this, lines are
/// still teed to tracing but dropped from the buffer.
const STDERR_BUFFER_CAP: usize = 8_192;

/// How long to wait for `<bin> --version` to print. Stuck binaries
/// (NFS hang, broken loader) error fast rather than blocking boot.
pub(crate) const VERSION_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolve the binary path: explicit override when set, otherwise
/// PATH-walk for `binary_name`. Explicit overrides MUST be absolute —
/// relative paths resolve against whichever cwd happens to be in
/// effect (different for `aura external-agent setup` vs the gateway
/// service), which would let probe succeed under one and fail under
/// the other.
pub(crate) fn resolve_binary(
    configured: Option<&str>,
    binary_name: &str,
    install_hint: &str,
) -> Result<PathBuf> {
    if let Some(explicit) = configured {
        let path = PathBuf::from(explicit);
        if !path.is_absolute() {
            return Err(ExternalAgentError::Config(format!(
                "{binary_name}: `binary_path` {explicit:?} must be an absolute path. Relative \
                 paths resolve against whichever cwd the process has (CLI vs gateway service \
                 differ). Use an absolute path, or leave the field empty to fall back to PATH \
                 lookup."
            )));
        }
        if !path.exists() {
            return Err(ExternalAgentError::NotInstalled(format!(
                "{binary_name}: `binary_path` {explicit:?} does not exist; either fix the path or \
                 leave it empty to fall back to PATH lookup"
            )));
        }
        return Ok(path);
    }
    which(binary_name).ok_or_else(|| {
        ExternalAgentError::NotInstalled(format!(
            "{binary_name}: no `{binary_name}` binary found in PATH. {install_hint}"
        ))
    })
}

/// Exec the binary to catch "exists but broken" (wrong arch, libc
/// mismatch, corrupt download).
pub(crate) fn check_binary_runs(binary: &PathBuf, binary_name: &str) -> Result<()> {
    // ETXTBSY (errno 26): kernel rejects exec while any writable fd
    // is open against the inode. Hits when an editor / installer just
    // wrote the binary, and concurrent test fixtures racing on shims.
    let mut attempts = 0;
    let output = loop {
        match run_with_timeout(Command::new(binary).arg("--version"), VERSION_CHECK_TIMEOUT) {
            Ok(out) => break out,
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && attempts < 5 => {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => {
                return Err(ExternalAgentError::Config(format!(
                    "{binary_name}: failed to run `{} --version`: {e}",
                    binary.display()
                )));
            }
        }
    };

    if !output.status.success() {
        return Err(ExternalAgentError::Config(format!(
            "{binary_name}: `{} --version` exited with {}: {}",
            binary.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        return Err(ExternalAgentError::Config(format!(
            "{binary_name}: `{} --version` printed nothing — the binary may be a wrapper that \
             expects a TTY or it may be the wrong tool. Try running it manually.",
            binary.display(),
        )));
    }
    Ok(())
}

fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> std::io::Result<std::process::Output> {
    // Synchronous (probe runs sync inside `probe_and_build`); tokio
    // timeout would force async up the chain for one shell-out.
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .spawn()?;
    let start = std::time::Instant::now();
    let poll_interval = Duration::from_millis(50);
    loop {
        match child.try_wait()? {
            Some(_) => return child.wait_with_output(),
            None => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("did not exit within {timeout:?}"),
                    ));
                }
                std::thread::sleep(poll_interval);
            }
        }
    }
}

pub(crate) fn which(binary_name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(binary_name);
        if candidate.is_file()
            && let Ok(meta) = std::fs::metadata(&candidate)
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 != 0 {
                return Some(candidate);
            }
        }
    }
    None
}

/// Create the per-spawn workspace dir if absent. `agent_name` is the
/// short label used in any error message.
pub(crate) async fn ensure_workspace_dir(dir: &Path, agent_name: &str) -> Result<()> {
    tokio::fs::create_dir_all(dir).await.map_err(|e| {
        ExternalAgentError::Config(format!(
            "{agent_name}: create workspace dir {}: {e}",
            dir.display()
        ))
    })
}

/// Take the captured `stdout` / `stderr` halves off a piped child, or
/// emit a `Transient` error if either is missing. Both stream parsers
/// need both halves with the same failure shape.
pub(crate) fn take_child_io(
    child: &mut Child,
    agent_name: &str,
) -> Result<(ChildStdout, ChildStderr)> {
    let stdout = child.stdout.take().ok_or_else(|| {
        ExternalAgentError::Transient(format!("{agent_name}: stdout not captured"))
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        ExternalAgentError::Transient(format!("{agent_name}: stderr not captured"))
    })?;
    Ok((stdout, stderr))
}

/// Tee the child's stderr to tracing (target `external_agent_stderr`)
/// and capture the first `STDERR_BUFFER_CAP` bytes into a shared
/// buffer so the caller can fold them into a non-zero-exit error
/// message. Returns the buffer handle.
pub(crate) fn spawn_stderr_tee(
    stderr: ChildStderr,
    agent_name: &'static str,
) -> Arc<Mutex<String>> {
    let buf = Arc::new(Mutex::new(String::new()));
    let buf_clone = Arc::clone(&buf);
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::warn!(target: "external_agent_stderr", agent = agent_name, "{line}");
            let mut b = buf_clone.lock();
            if b.len() < STDERR_BUFFER_CAP {
                b.push_str(&line);
                b.push('\n');
            }
        }
    });
    buf
}

/// Reap the child after the event stream closes naturally (stdout
/// EOF). Yields an error variant matching what the per-agent stream
/// parsers used to inline. Returns `Ok(())` when reap succeeds with
/// a 0 status, `Err` otherwise.
pub(crate) async fn reap_after_stream_close(
    child: &mut Child,
    agent_name: &str,
    stderr_buf: &Arc<Mutex<String>>,
) -> Result<()> {
    match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(status)) if !status.success() => {
            let snapshot = stderr_buf.lock().clone();
            Err(ExternalAgentError::Transient(format!(
                "{agent_name}: process exited {} with stderr: {}",
                status,
                snapshot.trim()
            )))
        }
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(ExternalAgentError::Transient(format!(
            "{agent_name}: wait: {e}"
        ))),
        Err(_) => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(KILL_GRACE, child.wait()).await;
            Ok(())
        }
    }
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    /// Build a fake binary at <tempdir>/<name> that prints `version_line`
    /// and exits 0. Caller must keep the `TempDir` alive for the duration
    /// of the test (drop = cleanup).
    pub(crate) fn fake_binary(name: &str, version_line: &str) -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join(name);
        std::fs::write(&bin, format!("#!/bin/sh\necho '{version_line}'\n")).unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();
        (dir, bin)
    }
}
