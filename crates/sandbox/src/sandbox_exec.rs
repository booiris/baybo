#![cfg(all(target_os = "macos", feature = "macos"))]

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::SandboxRunner;
use crate::args::{effective_pwd, render_sbpl_profile};
use crate::bootstrap::{SandboxAvailability, locate_binary, parse_version};
use crate::error::SandboxError;
use crate::spec::{Backend, EnvPolicy, FilesystemPolicy, SandboxOutput, SandboxSpec, StdinSource};

pub struct SandboxExecRunner {
    process_manager: Arc<baybo_process::ProcessManager>,
    binary: PathBuf,
}

impl SandboxExecRunner {
    pub fn discover(
        process_manager: Arc<baybo_process::ProcessManager>,
    ) -> Result<Self, SandboxError> {
        let binary = locate_binary("sandbox-exec")?;
        Ok(Self {
            process_manager,
            binary,
        })
    }

    pub async fn probe(
        process_manager: Arc<baybo_process::ProcessManager>,
    ) -> Result<SandboxAvailability, SandboxError> {
        let binary = locate_binary("sandbox-exec")?;
        let mut command = Command::new(&binary);
        command
            .arg("-h")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let out = match process_manager.spawn(&mut command, "sandbox-probe:sandbox-exec") {
            Ok(child) => child.wait_with_output().await.ok(),
            Err(_) => None,
        };
        let version = out.as_ref().and_then(|o| parse_version(&o.stdout));
        Ok(SandboxAvailability {
            backend: Backend::SandboxExec,
            binary_path: binary,
            version,
        })
    }

    /// Per-call scratch dir + the `$TMPDIR` to route at. Workspace policy
    /// carves a tempdir under the workspace (the handle is returned so the
    /// caller keeps it alive for the whole run / detached child); Permissive
    /// points straight at host `/tmp` (no handle — `/tmp`'s lifetime is the
    /// host's). Shared by `run` and `spawn_detached`.
    fn make_scratch(
        &self,
        spec: &SandboxSpec,
    ) -> Result<(Option<tempfile::TempDir>, PathBuf), SandboxError> {
        match &spec.filesystem_policy {
            FilesystemPolicy::Workspace => {
                let scratch = tempfile::Builder::new()
                    .prefix(".baybo-sandbox-")
                    .tempdir_in(&spec.workspace_root)?;
                let path = scratch.path().to_path_buf();
                Ok((Some(scratch), path))
            }
            FilesystemPolicy::Permissive { .. } => Ok((None, PathBuf::from("/tmp"))),
        }
    }

    /// Build the (unspawned) sandbox-exec `Command` for a spec + rendered SBPL
    /// profile, routing temp env vars at `tmpdir`. Shared by `run` and
    /// `spawn_detached`.
    fn build_command(
        &self,
        spec: &SandboxSpec,
        profile: &str,
        tmpdir: &std::path::Path,
    ) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.arg("-p").arg(profile);
        cmd.arg("env").arg("-i");
        cmd.arg("PATH=/usr/bin:/bin:/usr/sbin:/sbin");
        cmd.arg(format!("HOME={}", spec.workspace_root.display()));
        cmd.arg(format!("PWD={}", effective_pwd(spec)));
        cmd.arg(format!("TMPDIR={}", tmpdir.display()));
        cmd.arg(format!("TMP={}", tmpdir.display()));
        cmd.arg(format!("TEMP={}", tmpdir.display()));
        for opt in ["LANG", "LC_ALL", "TZ"] {
            if let Ok(v) = std::env::var(opt) {
                cmd.arg(format!("{opt}={v}"));
            }
        }
        if let EnvPolicy::Allowlist { vars } = &spec.env {
            for k in vars {
                if let Ok(v) = std::env::var(k) {
                    cmd.arg(format!("{k}={v}"));
                }
            }
        }
        if let EnvPolicy::BaselineWithExtra { extra } = &spec.env {
            for (k, v) in extra {
                cmd.arg(format!("{k}={v}"));
            }
        }
        cmd.arg("--").arg(&spec.program);
        for a in &spec.args {
            cmd.arg(a);
        }
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        } else {
            cmd.current_dir(&spec.workspace_root);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.stdin(match spec.stdin {
            StdinSource::Null => Stdio::null(),
            StdinSource::Inherit => Stdio::inherit(),
            StdinSource::Bytes(_) => Stdio::piped(),
        });
        cmd
    }

    /// Spawn the command, wiring `Bytes` stdin via a writer task. Shared by
    /// `run` and `spawn_detached`.
    fn spawn_with_stdin(
        &self,
        mut cmd: Command,
        stdin: &StdinSource,
    ) -> Result<baybo_process::ManagedChild, SandboxError> {
        let mut child = self
            .process_manager
            .spawn(&mut cmd, "sandbox:sandbox-exec")?;
        if let StdinSource::Bytes(bytes) = stdin
            && let Some(mut handle) = child.take_stdin()
        {
            let bytes = bytes.clone();
            tokio::spawn(async move {
                let _ = handle.write_all(&bytes).await;
            });
        }
        Ok(child)
    }
}

/// Pure validation of a `SandboxSpec` against sandbox-exec's
/// enforcement capabilities. SBPL has no equivalent for cgroup
/// memory/pid caps, so any non-`unlimited()` `resource_limits` is
/// unenforceable here. Refuse rather than silently downgrade — the
/// caller has either set caps deliberately and needs them, or has
/// not thought about it and should be made aware.
pub(crate) fn validate_spec(spec: &SandboxSpec) -> Result<(), SandboxError> {
    if !spec.resource_limits.is_unlimited() {
        return Err(SandboxError::Unenforceable {
            backend: "sandbox-exec",
            what: format!(
                "resource_limits {:?} (SBPL has no cgroup equivalent)",
                spec.resource_limits
            ),
            hint: "use the bwrap (Linux) or docker backend, layer a launchd MemoryHighWaterMark, or pass `ResourceLimits::unlimited()`",
        });
    }
    Ok(())
}

#[async_trait]
impl SandboxRunner for SandboxExecRunner {
    async fn run(&self, spec: SandboxSpec) -> Result<SandboxOutput, SandboxError> {
        validate_spec(&spec)?;
        let workspace_symlink_mount = spec
            .cwd
            .as_deref()
            .and_then(|cwd| crate::workspace_symlink_mount_for(cwd, &spec.workspace_root));
        let profile = render_sbpl_profile(&spec, workspace_symlink_mount.as_ref());
        // Workspace policy carves a per-call scratch dir under the workspace
        // and routes `$TMPDIR`/`$TMP`/`$TEMP` there (the handle is held until
        // `run()` returns; Drop removes it). Permissive (Bash) points
        // `$TMPDIR` straight at host `/tmp` — no handle. See `make_scratch`.
        let (scratch_handle, tmpdir) = self.make_scratch(&spec)?;
        let cmd = self.build_command(&spec, &profile, &tmpdir);

        let started = Instant::now();
        let child = self.spawn_with_stdin(cmd, &spec.stdin)?;

        let wait = child.wait_with_output();
        let result = tokio::time::timeout(spec.timeout, wait).await;
        let elapsed = started.elapsed();

        let outcome = match result {
            Ok(Ok(out)) => Ok(SandboxOutput {
                exit_code: out.status.code().unwrap_or(-1),
                stdout: out.stdout,
                stderr: out.stderr,
                elapsed,
                timed_out: false,
            }),
            Ok(Err(e)) => Err(SandboxError::Io(e)),
            Err(_) => Err(SandboxError::Timeout(spec.timeout)),
        };
        // Hold `scratch_handle` until after the child is awaited so the
        // per-call tempdir lives for the full lifetime of the sandboxed call.
        drop(scratch_handle);
        outcome
    }

    async fn spawn_detached(
        &self,
        spec: SandboxSpec,
    ) -> Result<Box<dyn crate::DetachedChild>, SandboxError> {
        validate_spec(&spec)?;
        let workspace_symlink_mount = spec
            .cwd
            .as_deref()
            .and_then(|cwd| crate::workspace_symlink_mount_for(cwd, &spec.workspace_root));
        let profile = render_sbpl_profile(&spec, workspace_symlink_mount.as_ref());
        // The per-call scratch tempdir (Workspace policy) must outlive the
        // detached child, so the returned `DetachedChild` holds the handle
        // instead of dropping it when this returns.
        let (scratch_handle, tmpdir) = self.make_scratch(&spec)?;
        let cmd = self.build_command(&spec, &profile, &tmpdir);
        let child = self.spawn_with_stdin(cmd, &spec.stdin)?;
        Ok(Box::new(SandboxExecDetachedChild {
            child,
            _scratch: scratch_handle,
        }))
    }

    fn backend(&self) -> Backend {
        Backend::SandboxExec
    }
}

/// A detached sandbox-exec child. Holds the per-call scratch tempdir
/// (Workspace policy) so it outlives the child rather than being removed when
/// `spawn_detached` returns.
struct SandboxExecDetachedChild {
    child: baybo_process::ManagedChild,
    _scratch: Option<tempfile::TempDir>,
}

#[async_trait]
impl crate::DetachedChild for SandboxExecDetachedChild {
    fn take_stdout(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
        self.child
            .take_stdout()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>)
    }
    fn take_stderr(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
        self.child
            .take_stderr()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>)
    }
    async fn wait(&mut self) -> i32 {
        self.child
            .wait()
            .await
            .ok()
            .and_then(|s| s.code())
            .unwrap_or(-1)
    }
    fn start_kill(&mut self) {
        let _ = self.child.start_kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{NetworkPolicy, ResourceLimits, StdinSource};
    use std::collections::BTreeSet;
    use std::time::Duration;

    fn baseline_spec() -> SandboxSpec {
        SandboxSpec {
            program: PathBuf::from("/usr/bin/true"),
            args: vec![],
            cwd: None,
            workspace_root: PathBuf::from("/tmp/ws"),
            readable_paths: vec![],
            writable_paths: vec![],
            allowed_hosts: BTreeSet::new(),
            network_policy: NetworkPolicy::None,
            env: EnvPolicy::Baseline,
            stdin: StdinSource::Null,
            timeout: Duration::from_secs(5),
            resource_limits: ResourceLimits::unlimited(),
            filesystem_policy: crate::spec::FilesystemPolicy::default(),
        }
    }

    #[test]
    fn validate_spec_rejects_resource_limits() {
        let mut spec = baseline_spec();
        spec.resource_limits = ResourceLimits::safe_defaults();
        let err = validate_spec(&spec).expect_err("must refuse");
        let SandboxError::Unenforceable { backend, what, .. } = err else {
            panic!("expected Unenforceable variant");
        };
        assert_eq!(backend, "sandbox-exec");
        assert!(what.contains("resource_limits"));
    }

    #[test]
    fn validate_spec_accepts_unlimited() {
        validate_spec(&baseline_spec()).expect("unlimited must pass on sandbox-exec");
    }
}
