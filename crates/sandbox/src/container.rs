use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{NetworkProxy, SandboxError, SandboxPolicy};

/// A job to be executed inside a container sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerJob {
    /// Command or entrypoint to run inside the container.
    pub command: Vec<String>,
    /// Working directory inside the container.
    pub workdir: Option<String>,
    /// Environment variables to inject.
    pub env: Vec<(String, String)>,
    /// JSON parameters passed to the job.
    pub params: Value,
    /// Timeout in milliseconds.
    pub timeout_ms: u64,
}

/// Result of a container execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerResult {
    /// Process exit code.
    pub exit_code: i32,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// Structured output, if any.
    pub output: Option<Value>,
}

/// Container-based execution sandbox (stub for Phase 1).
///
/// In a future phase this will manage container lifecycle via a
/// container runtime SDK or CLI.
#[derive(Debug)]
pub struct ContainerSandbox {
    _image: String,
    _network_proxy: Arc<NetworkProxy>,
}

impl ContainerSandbox {
    /// Create a new container sandbox targeting the given image.
    pub fn new(image: String, proxy: Arc<NetworkProxy>) -> Self {
        Self {
            _image: image,
            _network_proxy: proxy,
        }
    }

    /// Execute a job inside the container sandbox.
    ///
    /// Phase 1 stub: always returns [`SandboxError::ContainerNotImplemented`].
    pub async fn execute(
        &self,
        _job: ContainerJob,
        _policy: SandboxPolicy,
    ) -> Result<ContainerResult, SandboxError> {
        Err(SandboxError::ContainerNotImplemented)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NetworkPolicy;

    #[tokio::test]
    async fn execute_returns_not_implemented() {
        let proxy = Arc::new(NetworkProxy::new(NetworkPolicy {
            allowed_domains: vec![],
            allow_loopback: false,
        }));
        let sandbox = ContainerSandbox::new("alpine:latest".to_string(), proxy);
        let job = ContainerJob {
            command: vec!["echo".to_string(), "hello".to_string()],
            workdir: None,
            env: vec![],
            params: serde_json::json!({}),
            timeout_ms: 5000,
        };
        let result = sandbox
            .execute(job, SandboxPolicy::ContainerRestricted)
            .await;
        assert!(result.is_err());
        assert!(matches!(result, Err(SandboxError::ContainerNotImplemented)));
    }
}
