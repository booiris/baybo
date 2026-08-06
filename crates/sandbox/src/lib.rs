pub mod args;
pub mod bootstrap;
pub mod error;
pub mod spec;

mod symlink_mount;
pub use symlink_mount::{WorkspaceSymlinkMount, workspace_symlink_mount_for};

#[cfg(all(target_os = "linux", feature = "linux"))]
pub mod bwrap;
#[cfg(feature = "docker")]
pub mod docker;
#[cfg(all(target_os = "macos", feature = "macos"))]
pub mod sandbox_exec;

use std::sync::Arc;

use async_trait::async_trait;

pub use bootstrap::SandboxAvailability;
pub use error::SandboxError;
pub use spec::{
    Backend, EnvPolicy, FilesystemPolicy, NetworkPolicy, ResourceLimits, SandboxOutput,
    SandboxSpec, StdinSource, default_sensitive_denylist,
};

#[async_trait]
pub trait SandboxRunner: Send + Sync {
    async fn run(&self, spec: SandboxSpec) -> Result<SandboxOutput, SandboxError>;
    fn backend(&self) -> Backend;

    /// Drain backend-owned cleanup work before the process manager performs
    /// its final sweep. Backends without daemon-side resources are no-ops.
    async fn shutdown(&self, _deadline: tokio::time::Instant) {}

    /// Spawn the command **detached**: return a live [`DetachedChild`]
    /// immediately instead of running it to completion. The caller owns the
    /// foreground wait, timeout, and output streaming — used by the agent's
    /// "Bash timeout → background" path. `spec.timeout` is ignored (the
    /// caller times out). Default: unsupported; the agent's `SandboxAdapter`
    /// surfaces the error and the tool falls back to the blocking `run`
    /// (kill-on-timeout). bwrap, Docker, and sandbox-exec all override this
    /// (the sandbox-exec child holds its per-call scratch tempdir alive).
    async fn spawn_detached(
        &self,
        _spec: SandboxSpec,
    ) -> Result<Box<dyn DetachedChild>, SandboxError> {
        Err(SandboxError::InvalidSpec(
            "detached spawn is not supported by this sandbox backend".into(),
        ))
    }

    /// Optional readiness step run once at gateway startup before any
    /// `run()` calls. The Docker backend uses it to ensure the
    /// configured image is present locally and to pin its digest, so
    /// per-call `docker run` is hermetic and can use `--pull=never`.
    /// bwrap and sandbox-exec have no startup work and inherit the
    /// default no-op.
    async fn warm(&self) -> Result<(), SandboxError> {
        Ok(())
    }

    /// Per-host-and-backend safe defaults: the conservative resource
    /// caps this runner can actually enforce on the current host.
    /// Callers like the agent's `SandboxAdapter` use this so the
    /// per-call default is automatically tuned to the runner's
    /// capability instead of always asking for caps that some
    /// backends would have to refuse.
    ///
    /// Default implementation returns `ResourceLimits::unlimited()`,
    /// which is safe to pass to every backend. Runners that can do
    /// better override this.
    fn default_resource_limits(&self) -> spec::ResourceLimits {
        spec::ResourceLimits::unlimited()
    }
}

/// A live, detached sandboxed child the caller streams + awaits out of band
/// (the agent's "Bash timeout → background" path). Mirrors
/// `baybo_tools::RunningChild` — the agent's `SandboxAdapter` wraps one in the
/// other — and is defined here so `baybo-sandbox` stays free of an `baybo-tools`
/// dependency. Backends own the kill semantics: bwrap just signals the child;
/// the Docker backend must `docker rm -f` the container (a SIGKILL of the
/// `docker run` client would orphan it).
#[async_trait]
pub trait DetachedChild: Send {
    /// Take the child's stdout reader (once); `None` if already taken.
    fn take_stdout(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>;
    /// Take the child's stderr reader (once).
    fn take_stderr(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>;
    /// Wait for the child to exit; returns its exit code (or -1).
    async fn wait(&mut self) -> i32;
    /// Terminate the child (best-effort) — used by `/stop` / `JobStop`.
    fn start_kill(&mut self);
}

/// [`DetachedChild`] over a plain `tokio::process::Child` — bwrap (and the
/// container-less backends). The Docker backend uses its own type that also
/// removes the container on kill.
pub struct TokioDetachedChild(pub baybo_process::ManagedChild);

#[async_trait]
impl DetachedChild for TokioDetachedChild {
    fn take_stdout(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
        self.0
            .take_stdout()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>)
    }
    fn take_stderr(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
        self.0
            .take_stderr()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>)
    }
    async fn wait(&mut self) -> i32 {
        self.0
            .wait()
            .await
            .ok()
            .and_then(|s| s.code())
            .unwrap_or(-1)
    }
    fn start_kill(&mut self) {
        let _ = self.0.start_kill();
    }
}

/// Pick a sandbox runner appropriate for the current host.
///
/// Selection order:
///
/// 1. Native backend for the current `target_os` (bwrap on Linux,
///    sandbox-exec on macOS), if its feature is enabled and the
///    binary is on `$PATH`.
/// 2. Docker as a cross-platform fallback (if the `docker` feature
///    is enabled and the `docker` binary is on `$PATH`).
/// 3. Otherwise [`SandboxError::NoBackendAvailable`].
///
/// `BackendNotExecutable`, `Io`, and other non-`BackendMissing` errors
/// are returned immediately and do not trigger fallback — they signal
/// a real problem with the chosen backend rather than its absence.
pub async fn current_platform_runner(
    process_manager: Arc<baybo_process::ProcessManager>,
) -> Result<Arc<dyn SandboxRunner>, SandboxError> {
    #[cfg(all(target_os = "linux", feature = "linux"))]
    {
        match bwrap::BwrapRunner::discover(Arc::clone(&process_manager)).await {
            Ok(r) => return Ok(Arc::new(r)),
            Err(SandboxError::BackendMissing { .. } | SandboxError::BackendUnreachable { .. }) => {
                tracing::warn!("bwrap not usable; trying docker fallback");
            }
            Err(e) => return Err(e),
        }
    }
    #[cfg(all(target_os = "macos", feature = "macos"))]
    {
        match sandbox_exec::SandboxExecRunner::discover(Arc::clone(&process_manager)) {
            Ok(r) => return Ok(Arc::new(r)),
            Err(SandboxError::BackendMissing { .. } | SandboxError::BackendUnreachable { .. }) => {
                tracing::warn!("sandbox-exec not usable; trying docker fallback");
            }
            Err(e) => return Err(e),
        }
    }
    #[cfg(feature = "docker")]
    {
        match docker::DockerRunner::discover(process_manager).await {
            Ok(r) => {
                tracing::info!("using docker as the sandbox backend");
                return Ok(Arc::new(r));
            }
            Err(SandboxError::BackendMissing { .. } | SandboxError::BackendUnreachable { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    Err(SandboxError::NoBackendAvailable)
}

pub async fn probe() -> Result<SandboxAvailability, SandboxError> {
    let process_manager = baybo_process::ProcessManager::transient();
    #[cfg(all(target_os = "linux", feature = "linux"))]
    {
        match bwrap::BwrapRunner::probe(Arc::clone(&process_manager)).await {
            Ok(a) => return Ok(a),
            Err(SandboxError::BackendMissing { .. } | SandboxError::BackendUnreachable { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    #[cfg(all(target_os = "macos", feature = "macos"))]
    {
        match sandbox_exec::SandboxExecRunner::probe(Arc::clone(&process_manager)).await {
            Ok(a) => return Ok(a),
            Err(SandboxError::BackendMissing { .. } | SandboxError::BackendUnreachable { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    #[cfg(feature = "docker")]
    {
        match docker::DockerRunner::probe(process_manager).await {
            Ok(a) => return Ok(a),
            Err(SandboxError::BackendMissing { .. } | SandboxError::BackendUnreachable { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    Err(SandboxError::NoBackendAvailable)
}
