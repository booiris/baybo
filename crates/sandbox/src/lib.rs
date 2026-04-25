pub mod args;
pub mod bootstrap;
pub mod error;
pub mod spec;

#[cfg(all(target_os = "linux", feature = "linux"))]
pub mod bwrap;
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
}

pub fn current_platform_runner() -> Result<Arc<dyn SandboxRunner>, SandboxError> {
    #[cfg(all(target_os = "linux", feature = "linux"))]
    {
        Ok(Arc::new(bwrap::BwrapRunner::discover()?))
    }
    #[cfg(all(target_os = "macos", feature = "macos"))]
    {
        Ok(Arc::new(sandbox_exec::SandboxExecRunner::discover()?))
    }
    #[cfg(not(any(
        all(target_os = "linux", feature = "linux"),
        all(target_os = "macos", feature = "macos"),
    )))]
    {
        Err(SandboxError::UnsupportedPlatform(std::env::consts::OS))
    }
}

pub async fn probe() -> Result<SandboxAvailability, SandboxError> {
    #[cfg(all(target_os = "linux", feature = "linux"))]
    {
        bwrap::BwrapRunner::probe().await
    }
    #[cfg(all(target_os = "macos", feature = "macos"))]
    {
        sandbox_exec::SandboxExecRunner::probe().await
    }
    #[cfg(not(any(
        all(target_os = "linux", feature = "linux"),
        all(target_os = "macos", feature = "macos"),
    )))]
    {
        Err(SandboxError::UnsupportedPlatform(std::env::consts::OS))
    }
}
