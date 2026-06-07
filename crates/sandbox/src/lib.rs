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

    /// Spawn the command **detached**: return a live `tokio::process::Child`
    /// immediately instead of running it to completion. The caller owns the
    /// foreground wait, timeout, and output streaming — used by the agent's
    /// "Bash timeout → background" path. `spec.timeout` is ignored (the
    /// caller times out). Default: unsupported; the agent's `SandboxAdapter`
    /// surfaces the error and the tool falls back to the blocking `run`
    /// (kill-on-timeout). Backends that own a `tokio::process::Child`
    /// (bwrap, sandbox-exec) override this; the Docker backend does not.
    async fn spawn_detached(
        &self,
        _spec: SandboxSpec,
    ) -> Result<tokio::process::Child, SandboxError> {
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
pub fn current_platform_runner() -> Result<Arc<dyn SandboxRunner>, SandboxError> {
    #[cfg(all(target_os = "linux", feature = "linux"))]
    {
        match bwrap::BwrapRunner::discover() {
            Ok(r) => return Ok(Arc::new(r)),
            Err(SandboxError::BackendMissing { .. } | SandboxError::BackendUnreachable { .. }) => {
                tracing::warn!("bwrap not usable; trying docker fallback");
            }
            Err(e) => return Err(e),
        }
    }
    #[cfg(all(target_os = "macos", feature = "macos"))]
    {
        match sandbox_exec::SandboxExecRunner::discover() {
            Ok(r) => return Ok(Arc::new(r)),
            Err(SandboxError::BackendMissing { .. } | SandboxError::BackendUnreachable { .. }) => {
                tracing::warn!("sandbox-exec not usable; trying docker fallback");
            }
            Err(e) => return Err(e),
        }
    }
    #[cfg(feature = "docker")]
    {
        match docker::DockerRunner::discover() {
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
    #[cfg(all(target_os = "linux", feature = "linux"))]
    {
        match bwrap::BwrapRunner::probe().await {
            Ok(a) => return Ok(a),
            Err(SandboxError::BackendMissing { .. } | SandboxError::BackendUnreachable { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    #[cfg(all(target_os = "macos", feature = "macos"))]
    {
        match sandbox_exec::SandboxExecRunner::probe().await {
            Ok(a) => return Ok(a),
            Err(SandboxError::BackendMissing { .. } | SandboxError::BackendUnreachable { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    #[cfg(feature = "docker")]
    {
        match docker::DockerRunner::probe().await {
            Ok(a) => return Ok(a),
            Err(SandboxError::BackendMissing { .. } | SandboxError::BackendUnreachable { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    Err(SandboxError::NoBackendAvailable)
}
