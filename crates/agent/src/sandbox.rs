use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aura_sandbox::{EnvPolicy, NetworkPolicy, SandboxRunner, SandboxSpec, StdinSource};
use aura_tools::{ExecSandbox, SandboxedOutput, ToolError};

pub struct SandboxAdapter {
    runner: Arc<dyn SandboxRunner>,
    workspace_root: PathBuf,
    network_policy: NetworkPolicy,
}

impl SandboxAdapter {
    pub fn new(
        runner: Arc<dyn SandboxRunner>,
        workspace_root: PathBuf,
        network_policy: NetworkPolicy,
    ) -> Self {
        Self {
            runner,
            workspace_root,
            network_policy,
        }
    }
}

#[async_trait]
impl ExecSandbox for SandboxAdapter {
    async fn spawn_command(
        &self,
        program: &Path,
        args: &[String],
        cwd: Option<&Path>,
        stdin: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<SandboxedOutput, ToolError> {
        if let Some(requested) = cwd {
            // Resolve symlinks on both sides before comparing — on macOS
            // `/tmp` resolves to `/private/tmp`, which would otherwise
            // make a perfectly-valid cwd look like an escape attempt.
            let canon_cwd = requested.canonicalize().map_err(|e| {
                ToolError::InvalidParams(format!(
                    "cwd `{}` does not exist or cannot be resolved: {}",
                    requested.display(),
                    e
                ))
            })?;
            let canon_root = self
                .workspace_root
                .canonicalize()
                .unwrap_or_else(|_| self.workspace_root.clone());
            if !canon_cwd.starts_with(&canon_root) {
                return Err(ToolError::InvalidParams(format!(
                    "cwd `{}` must be inside workspace root `{}`",
                    requested.display(),
                    canon_root.display()
                )));
            }
        }

        let spec = SandboxSpec {
            program: program.to_path_buf(),
            args: args.to_vec(),
            cwd: cwd.map(Path::to_path_buf),
            workspace_root: self.workspace_root.clone(),
            readable_paths: Vec::new(),
            allowed_hosts: BTreeSet::new(),
            network_policy: self.network_policy,
            env: EnvPolicy::Baseline,
            stdin: match stdin {
                Some(b) => StdinSource::Bytes(b.to_vec()),
                None => StdinSource::Null,
            },
            timeout,
        };

        match self.runner.run(spec).await {
            Ok(out) => Ok(SandboxedOutput {
                exit_code: out.exit_code,
                stdout: out.stdout,
                stderr: out.stderr,
                timed_out: out.timed_out,
            }),
            Err(aura_sandbox::SandboxError::Timeout(_)) => Ok(SandboxedOutput {
                exit_code: -1,
                stdout: Vec::new(),
                stderr: Vec::new(),
                timed_out: true,
            }),
            Err(e) => Err(ToolError::Execution(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_sandbox::{Backend, SandboxError, SandboxOutput};
    use parking_lot::Mutex;

    struct PanicRunner;

    #[async_trait]
    impl SandboxRunner for PanicRunner {
        async fn run(&self, _: SandboxSpec) -> Result<SandboxOutput, SandboxError> {
            panic!("runner must not be invoked when cwd validation fails");
        }
        fn backend(&self) -> Backend {
            Backend::Bwrap
        }
    }

    #[derive(Default)]
    struct RecordingRunner {
        seen: Mutex<Option<SandboxSpec>>,
    }

    #[async_trait]
    impl SandboxRunner for RecordingRunner {
        async fn run(&self, spec: SandboxSpec) -> Result<SandboxOutput, SandboxError> {
            *self.seen.lock() = Some(spec);
            Ok(SandboxOutput {
                exit_code: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
                elapsed: Duration::from_millis(1),
                timed_out: false,
            })
        }
        fn backend(&self) -> Backend {
            Backend::Bwrap
        }
    }

    #[tokio::test]
    async fn rejects_cwd_outside_workspace() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        let adapter = SandboxAdapter::new(
            Arc::new(PanicRunner),
            workspace.path().to_path_buf(),
            NetworkPolicy::None,
        );
        let res = adapter
            .spawn_command(
                Path::new("/bin/echo"),
                &["hi".into()],
                Some(outside.path()),
                None,
                Duration::from_secs(1),
            )
            .await;
        assert!(
            matches!(res, Err(ToolError::InvalidParams(ref m)) if m.contains("workspace")),
            "expected InvalidParams about workspace, got: {res:?}",
        );
    }

    #[tokio::test]
    async fn accepts_cwd_inside_workspace() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let sub = workspace.path().join("sub");
        std::fs::create_dir(&sub).expect("create subdir");
        let runner = Arc::new(RecordingRunner::default());
        let adapter = SandboxAdapter::new(
            Arc::clone(&runner) as Arc<dyn SandboxRunner>,
            workspace.path().to_path_buf(),
            NetworkPolicy::None,
        );
        let res = adapter
            .spawn_command(
                Path::new("/bin/echo"),
                &["hi".into()],
                Some(&sub),
                None,
                Duration::from_secs(1),
            )
            .await
            .expect("cwd inside workspace must be accepted");
        assert_eq!(res.exit_code, 0);
        assert!(runner.seen.lock().is_some(), "runner must have been called");
    }

    #[tokio::test]
    async fn rejects_cwd_that_does_not_exist() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let adapter = SandboxAdapter::new(
            Arc::new(PanicRunner),
            workspace.path().to_path_buf(),
            NetworkPolicy::None,
        );
        let missing = workspace.path().join("does-not-exist");
        let res = adapter
            .spawn_command(
                Path::new("/bin/echo"),
                &["hi".into()],
                Some(&missing),
                None,
                Duration::from_secs(1),
            )
            .await;
        assert!(
            matches!(res, Err(ToolError::InvalidParams(ref m)) if m.contains("does not exist")),
            "expected InvalidParams about missing path, got: {res:?}",
        );
    }
}
