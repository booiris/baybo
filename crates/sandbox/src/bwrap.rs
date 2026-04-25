#![cfg(all(target_os = "linux", feature = "linux"))]

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Instant;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::SandboxRunner;
use crate::args::build_bwrap_argv;
use crate::bootstrap::{SandboxAvailability, locate_binary, parse_version};
use crate::error::SandboxError;
use crate::spec::{Backend, SandboxOutput, SandboxSpec, StdinSource};

pub struct BwrapRunner {
    binary: PathBuf,
}

impl BwrapRunner {
    pub fn discover() -> Result<Self, SandboxError> {
        let binary = locate_binary("bwrap")?;
        Ok(Self { binary })
    }

    pub async fn probe() -> Result<SandboxAvailability, SandboxError> {
        let binary = locate_binary("bwrap")?;
        let out = Command::new(&binary).arg("--version").output().await?;
        Ok(SandboxAvailability {
            backend: Backend::Bwrap,
            binary_path: binary,
            version: parse_version(&out.stdout),
        })
    }
}

#[async_trait]
impl SandboxRunner for BwrapRunner {
    async fn run(&self, spec: SandboxSpec) -> Result<SandboxOutput, SandboxError> {
        let argv = build_bwrap_argv(&spec);
        let mut cmd = Command::new(&self.binary);
        cmd.args(&argv)
            .stdout(Stdio::piped())
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

        match result {
            Ok(Ok(out)) => {
                let exit_code = out.status.code().unwrap_or(-1);
                let stderr_str = String::from_utf8_lossy(&out.stderr);
                if !out.status.success() && stderr_str.starts_with("bwrap: ") {
                    return Err(SandboxError::BackendFailure {
                        status: out.status.code(),
                        stderr: stderr_str.into_owned(),
                    });
                }
                Ok(SandboxOutput {
                    exit_code,
                    stdout: out.stdout,
                    stderr: out.stderr,
                    elapsed,
                    timed_out: false,
                })
            }
            Ok(Err(e)) => Err(SandboxError::Io(e)),
            Err(_) => Err(SandboxError::Timeout(spec.timeout)),
        }
    }

    fn backend(&self) -> Backend {
        Backend::Bwrap
    }
}
