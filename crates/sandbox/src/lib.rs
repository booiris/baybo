pub mod args;
pub mod bootstrap;
pub mod error;
pub mod spec;

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
pub use spec::{Backend, EnvPolicy, NetworkPolicy, SandboxOutput, SandboxSpec, StdinSource};

#[async_trait]
pub trait SandboxRunner: Send + Sync {
    async fn run(&self, spec: SandboxSpec) -> Result<SandboxOutput, SandboxError>;
    fn backend(&self) -> Backend;

    /// Optional readiness step run once at gateway startup before any
    /// `run()` calls. The Docker backend uses it to ensure the
    /// configured image is present locally and to pin its digest, so
    /// per-call `docker run` is hermetic and can use `--pull=never`.
    /// bwrap and sandbox-exec have no startup work and inherit the
    /// default no-op.
    async fn warm(&self) -> Result<(), SandboxError> {
        Ok(())
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
