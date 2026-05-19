//! Shared subprocess helpers for external-agent impls — boot-time
//! probe (binary resolution + `<bin> --version` check), workspace
//! dir prep, and the cancel/timeout-aware `wait_with_output` shell
//! used by both run-time drivers. The per-agent drivers (claude_cli,
//! codex_cli) own the protocol-specific stdout parsing.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use tokio::process::Child;

use super::{ExternalAgentError, Result};

/// How long to wait for `<bin> --version` to print. Stuck binaries
/// (NFS hang, broken loader) error fast rather than blocking boot.
pub(crate) const VERSION_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolve the binary path: explicit override when set, otherwise
/// PATH-walk for `binary_name`.
pub(crate) fn resolve_binary(
    configured: Option<&str>,
    binary_name: &str,
    install_hint: &str,
) -> Result<PathBuf> {
    if let Some(explicit) = configured {
        let path = PathBuf::from(explicit);
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
        match run_with_timeout(
            Command::new(binary).arg("--version"),
            VERSION_CHECK_TIMEOUT,
        ) {
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

fn run_with_timeout(
    cmd: &mut Command,
    timeout: Duration,
) -> std::io::Result<std::process::Output> {
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

/// Run the child to completion, racing it against a cancel token and
/// a wall-clock timeout. The Child is moved in, so on cancel/timeout
/// the future drops it — `kill_on_drop` (which both drivers set when
/// building the Command) then sends SIGKILL. Returns the full
/// captured stdout+stderr+status on natural exit; otherwise a
/// `Transient` error with text the consumer recognises (`exceeded
/// declared timeout`, `cancelled by parent`) for status mapping.
pub(crate) async fn wait_with_cancel_timeout(
    child: Child,
    cancel: tokio_util::sync::CancellationToken,
    timeout: Duration,
    agent_name: &'static str,
) -> Result<Output> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(ExternalAgentError::Transient(format!(
            "{agent_name}: cancelled by parent"
        ))),
        _ = tokio::time::sleep(timeout) => Err(ExternalAgentError::Transient(format!(
            "{agent_name}: exceeded declared timeout"
        ))),
        result = child.wait_with_output() => result.map_err(|e| {
            ExternalAgentError::Transient(format!("{agent_name}: wait_with_output: {e}"))
        }),
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
