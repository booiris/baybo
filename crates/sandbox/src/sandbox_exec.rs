#![cfg(all(target_os = "macos", feature = "macos"))]

use std::path::PathBuf;
use std::process::Stdio;
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
    binary: PathBuf,
}

impl SandboxExecRunner {
    pub fn discover() -> Result<Self, SandboxError> {
        let binary = locate_binary("sandbox-exec")?;
        Ok(Self { binary })
    }

    pub async fn probe() -> Result<SandboxAvailability, SandboxError> {
        let binary = locate_binary("sandbox-exec")?;
        let out = Command::new(&binary).arg("-h").output().await.ok();
        let version = out.as_ref().and_then(|o| parse_version(&o.stdout));
        Ok(SandboxAvailability {
            backend: Backend::SandboxExec,
            binary_path: binary,
            version,
        })
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
        // Workspace policy: SBPL allows writes only inside the
        // workspace. Carve out a per-invocation scratch dir under
        // the workspace and route `$TMPDIR` / `$TMP` / `$TEMP` there
        // so callers respecting those env vars stay inside the bind.
        // The TempDir handle is held until `run()` returns; Drop
        // removes the directory.
        //
        // Permissive policy (Bash): the SBPL profile already grants
        // RW on `/private/tmp`, matching the bwrap `--bind /tmp /tmp`
        // shape on Linux. Skip the scratch dir and point `$TMPDIR`
        // straight at host `/tmp` so files persist across calls and
        // hardcoded `/tmp` writers work.
        let (scratch_handle, tmpdir) = match &spec.filesystem_policy {
            FilesystemPolicy::Workspace => {
                let scratch = tempfile::Builder::new()
                    .prefix(".aura-sandbox-")
                    .tempdir_in(&spec.workspace_root)?;
                let path = scratch.path().to_path_buf();
                (Some(scratch), path)
            }
            FilesystemPolicy::Permissive { .. } => (None, PathBuf::from("/tmp")),
        };

        let mut cmd = Command::new(&self.binary);
        cmd.arg("-p").arg(&profile);
        cmd.arg("env").arg("-i");
        cmd.arg("PATH=/usr/bin:/bin:/usr/sbin:/sbin");
        cmd.arg(format!("HOME={}", spec.workspace_root.display()));
        cmd.arg(format!("PWD={}", effective_pwd(&spec)));
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
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd.stdin(match spec.stdin {
            StdinSource::Null => Stdio::null(),
            StdinSource::Inherit => Stdio::inherit(),
            StdinSource::Bytes(_) => Stdio::piped(),
        });

        let started = Instant::now();
        let mut child = cmd.spawn()?;

        if let StdinSource::Bytes(bytes) = &spec.stdin
            && let Some(mut stdin) = child.stdin.take()
        {
            let bytes = bytes.clone();
            tokio::spawn(async move {
                let _ = stdin.write_all(&bytes).await;
            });
        }

        let wait = child.wait_with_output();
        let result = tokio::time::timeout(spec.timeout, wait).await;
        let elapsed = started.elapsed();

        // Hold `scratch_handle` (when present) until after the child
        // has been awaited so the per-call tempdir lives for the full
        // lifetime of the sandboxed call. Permissive mode has no
        // handle (`/tmp` is the host directory; lifetime is the
        // host's problem).
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
        drop(scratch_handle);
        outcome
    }

    fn backend(&self) -> Backend {
        Backend::SandboxExec
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
