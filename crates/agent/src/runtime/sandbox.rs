use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use baybo_sandbox::{
    EnvPolicy, FilesystemPolicy, NetworkPolicy, ResourceLimits, SandboxRunner, SandboxSpec,
    StdinSource,
};
use baybo_tools::{ExecSandbox, RunningChild, SandboxedOutput, SpawnOpts, ToolError};
use baybo_workspace::absolutise;

pub struct SandboxAdapter {
    runner: Arc<dyn SandboxRunner>,
    workspace_root: PathBuf,
    network_policy: NetworkPolicy,
    resource_limits: ResourceLimits,
    allowed_hosts: BTreeSet<String>,
    filesystem_policy: FilesystemPolicy,
    readable_paths: Vec<PathBuf>,
    cwd_must_be_in_workspace: bool,
}

impl SandboxAdapter {
    pub fn new(
        runner: Arc<dyn SandboxRunner>,
        workspace_root: PathBuf,
        network_policy: NetworkPolicy,
    ) -> Self {
        // Pull the per-call default from the runner so the adapter
        // doesn't ask for caps the chosen backend can't enforce. On
        // bwrap with systemd-run + Docker this is `safe_defaults()`
        // (512 MiB / 256 pids); on sandbox-exec or bwrap without
        // systemd-run it's `unlimited()`. Callers can still override
        // explicitly via `with_resource_limits`, in which case the
        // backend's `validate_spec` will fail-closed if the request
        // exceeds what it can deliver.
        let resource_limits = runner.default_resource_limits();
        Self {
            runner,
            workspace_root,
            network_policy,
            resource_limits,
            allowed_hosts: BTreeSet::new(),
            filesystem_policy: FilesystemPolicy::Workspace,
            readable_paths: Vec::new(),
            cwd_must_be_in_workspace: true,
        }
    }

    pub fn with_resource_limits(mut self, limits: ResourceLimits) -> Self {
        self.resource_limits = limits;
        self
    }

    pub fn with_allowed_hosts(mut self, hosts: BTreeSet<String>) -> Self {
        self.allowed_hosts = hosts;
        self
    }

    /// Switch to permissive filesystem mode. `extra_root` is bound RW
    /// alongside `workspace_root` (typically `$HOME`); FHS roots stay
    /// RO; everything outside that union remains invisible. Each
    /// `denied_paths` entry is then masked with a per-call empty tmpfs
    /// so credential vaults sitting inside `extra_root` still can't be
    /// read or written.
    ///
    /// `extra_root` and every denied path are resolved through their
    /// symlinks before reaching the backend. bwrap's `--tmpfs`/`--bind`
    /// follow a symlinked target *within the sandbox root*, so a state
    /// dir reached via a symlink to an unbound location (e.g. a default
    /// `~/.baybo` symlinked to a `/data/...` path outside the permissive
    /// bind set) resolves to a target that doesn't exist in the assembled
    /// root and aborts sandbox setup. Canonicalising also drops
    /// non-existent denied paths — a missing `--tmpfs` target is a hard
    /// bwrap error, not a no-op. The cwd-must-be-in-workspace check is
    /// relaxed because the agent's writable surface is now wider than
    /// `workspace_root`.
    pub fn with_permissive_filesystem(
        mut self,
        extra_root: PathBuf,
        denied_paths: Vec<PathBuf>,
    ) -> Self {
        let mut denied_paths = denied_paths
            .into_iter()
            .filter_map(|p| p.canonicalize().ok())
            .collect::<Vec<_>>();
        denied_paths.sort();
        denied_paths.dedup();
        self.filesystem_policy = FilesystemPolicy::Permissive {
            extra_root: absolutise(&extra_root),
            denied_paths,
        };
        self.cwd_must_be_in_workspace = false;
        self
    }

    /// Expose extra host paths **read-only** inside the sandbox, mounted
    /// at the same path. Used to surface `<workspace>/skills` so an
    /// installed skill's bundled script runs in place: under
    /// `Permissive`, the RO bind is layered on top of the denylist's
    /// masking tmpfs (the agent's denylist masks all of `~/.baybo`), so
    /// the script stays readable while the rest of the baybo state stays
    /// hidden. Paths are canonicalised (same symlinked-target reasoning
    /// as [`Self::with_permissive_filesystem`]) and non-existent ones are
    /// dropped — bwrap's `--ro-bind-try` tolerates a missing source, but
    /// Docker's `-v …:ro` would fail and an SBPL allow on a missing path
    /// is dead weight.
    pub fn with_readable_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.readable_paths = paths
            .into_iter()
            .filter_map(|p| p.canonicalize().ok())
            .collect();
        self
    }

    /// Validate the cwd and build the [`SandboxSpec`] — shared by the
    /// blocking [`ExecSandbox::spawn_command`] and the detached
    /// [`ExecSandbox::spawn_command_detached`] so both enforce the same
    /// FS scope and policy.
    fn build_spec(
        &self,
        program: &Path,
        args: &[String],
        opts: SpawnOpts,
    ) -> Result<SandboxSpec, ToolError> {
        if let Some(requested) = opts.cwd.as_deref()
            && self.cwd_must_be_in_workspace
        {
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

        let env = if opts.extra_env.is_empty() {
            EnvPolicy::Baseline
        } else {
            EnvPolicy::BaselineWithExtra {
                extra: opts.extra_env,
            }
        };
        Ok(SandboxSpec {
            program: program.to_path_buf(),
            args: args.to_vec(),
            cwd: opts.cwd,
            workspace_root: self.workspace_root.clone(),
            readable_paths: self.readable_paths.clone(),
            writable_paths: Vec::new(),
            allowed_hosts: self.allowed_hosts.clone(),
            network_policy: self.network_policy,
            env,
            stdin: match opts.stdin {
                Some(b) => StdinSource::Bytes(b),
                None => StdinSource::Null,
            },
            timeout: opts.timeout,
            resource_limits: self.resource_limits,
            filesystem_policy: self.filesystem_policy.clone(),
        })
    }
}

#[async_trait]
impl ExecSandbox for SandboxAdapter {
    async fn spawn_command(
        &self,
        program: &Path,
        args: &[String],
        opts: SpawnOpts,
    ) -> Result<SandboxedOutput, ToolError> {
        let spec = self.build_spec(program, args, opts)?;

        match self.runner.run(spec).await {
            Ok(out) => Ok(SandboxedOutput {
                exit_code: out.exit_code,
                stdout: out.stdout,
                stderr: out.stderr,
                timed_out: out.timed_out,
            }),
            Err(baybo_sandbox::SandboxError::Timeout(_)) => Ok(SandboxedOutput {
                exit_code: -1,
                stdout: Vec::new(),
                stderr: Vec::new(),
                timed_out: true,
            }),
            Err(e) => Err(ToolError::Execution(e.to_string())),
        }
    }

    async fn spawn_command_detached(
        &self,
        program: &Path,
        args: &[String],
        opts: SpawnOpts,
    ) -> Result<Box<dyn RunningChild>, ToolError> {
        let spec = self.build_spec(program, args, opts)?;
        match self.runner.spawn_detached(spec).await {
            Ok(detached) => Ok(Box::new(DetachedChildAdapter(detached))),
            Err(e) => Err(ToolError::Execution(e.to_string())),
        }
    }
}

/// Bridges a backend's [`baybo_sandbox::DetachedChild`] to the tool layer's
/// [`RunningChild`] (identical shape; the two traits live in different crates
/// to keep `baybo-sandbox` free of an `baybo-tools` dependency).
struct DetachedChildAdapter(Box<dyn baybo_sandbox::DetachedChild>);

#[async_trait]
impl RunningChild for DetachedChildAdapter {
    fn take_stdout(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
        self.0.take_stdout()
    }
    fn take_stderr(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
        self.0.take_stderr()
    }
    async fn wait(&mut self) -> i32 {
        self.0.wait().await
    }
    fn start_kill(&mut self) {
        self.0.start_kill()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_sandbox::{Backend, SandboxError, SandboxOutput};
    use parking_lot::Mutex;
    use std::time::Duration;

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
                SpawnOpts {
                    cwd: Some(outside.path().to_path_buf()),
                    timeout: Duration::from_secs(1),
                    ..Default::default()
                },
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
                SpawnOpts {
                    cwd: Some(sub.clone()),
                    timeout: Duration::from_secs(1),
                    ..Default::default()
                },
            )
            .await
            .expect("cwd inside workspace must be accepted");
        assert_eq!(res.exit_code, 0);
        assert!(runner.seen.lock().is_some(), "runner must have been called");
    }

    #[tokio::test]
    async fn resource_limits_and_allowed_hosts_round_trip_into_spec() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let runner = Arc::new(RecordingRunner::default());
        let limits = ResourceLimits {
            memory_max_bytes: Some(123_456_789),
            pids_max: Some(7),
        };
        let hosts = BTreeSet::from(["api.openai.com:443".to_string(), "github.com".to_string()]);
        let adapter = SandboxAdapter::new(
            Arc::clone(&runner) as Arc<dyn SandboxRunner>,
            workspace.path().to_path_buf(),
            NetworkPolicy::All,
        )
        .with_resource_limits(limits)
        .with_allowed_hosts(hosts.clone());
        adapter
            .spawn_command(
                Path::new("/bin/echo"),
                &["hi".into()],
                SpawnOpts {
                    timeout: Duration::from_secs(1),
                    ..Default::default()
                },
            )
            .await
            .expect("recording runner accepts");
        let seen = runner.seen.lock().take().expect("runner saw spec");
        assert_eq!(seen.resource_limits, limits);
        assert_eq!(seen.allowed_hosts, hosts);
    }

    #[tokio::test]
    async fn readable_paths_round_trip_into_spec_and_drop_missing() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let skills = tempfile::tempdir().expect("skills tempdir");
        let missing = workspace.path().join("does-not-exist");
        let runner = Arc::new(RecordingRunner::default());
        let adapter = SandboxAdapter::new(
            Arc::clone(&runner) as Arc<dyn SandboxRunner>,
            workspace.path().to_path_buf(),
            NetworkPolicy::All,
        )
        .with_readable_paths(vec![skills.path().to_path_buf(), missing.clone()]);
        adapter
            .spawn_command(
                Path::new("/bin/echo"),
                &["hi".into()],
                SpawnOpts {
                    timeout: Duration::from_secs(1),
                    ..Default::default()
                },
            )
            .await
            .expect("recording runner accepts");
        let seen = runner.seen.lock().take().expect("runner saw spec");
        assert_eq!(
            seen.readable_paths,
            vec![skills.path().canonicalize().expect("canonicalize skills")],
            "existing readable path must round-trip (canonicalised); missing path must be dropped"
        );
    }

    #[tokio::test]
    async fn permissive_denied_symlink_resolves_to_real_target() {
        // Reproduces the `~/.baybo -> /data/.../.baybo` shape: a denied
        // path reached via a symlink must be handed to the backend as its
        // real target, otherwise bwrap's `--tmpfs` follows the link into
        // an unbound location and fails sandbox setup.
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let real_state = tempfile::tempdir().expect("state tempdir");
        let link = workspace.path().join("state-link");
        std::os::unix::fs::symlink(real_state.path(), &link).expect("symlink");
        let runner = Arc::new(RecordingRunner::default());
        let adapter = SandboxAdapter::new(
            Arc::clone(&runner) as Arc<dyn SandboxRunner>,
            workspace.path().to_path_buf(),
            NetworkPolicy::All,
        )
        .with_permissive_filesystem(workspace.path().to_path_buf(), vec![link]);
        adapter
            .spawn_command(
                Path::new("/bin/echo"),
                &["hi".into()],
                SpawnOpts {
                    timeout: Duration::from_secs(1),
                    ..Default::default()
                },
            )
            .await
            .expect("recording runner accepts");
        let seen = runner.seen.lock().take().expect("runner saw spec");
        let FilesystemPolicy::Permissive { denied_paths, .. } = seen.filesystem_policy else {
            panic!("expected permissive filesystem policy");
        };
        assert_eq!(
            denied_paths,
            vec![
                real_state
                    .path()
                    .canonicalize()
                    .expect("canonicalize state")
            ],
            "denied symlink must be resolved to its real target for --tmpfs",
        );
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
                SpawnOpts {
                    cwd: Some(missing.clone()),
                    timeout: Duration::from_secs(1),
                    ..Default::default()
                },
            )
            .await;
        assert!(
            matches!(res, Err(ToolError::InvalidParams(ref m)) if m.contains("does not exist")),
            "expected InvalidParams about missing path, got: {res:?}",
        );
    }
}
