mod container;
mod network;
mod wasm;

pub use container::{ContainerJob, ContainerResult, ContainerSandbox};
pub use network::{NetworkPolicy, NetworkProxy};
pub use wasm::{SandboxLimits, WasmModule, WasmRuntime};

use serde::{Deserialize, Serialize};

/// Execution isolation policy selected by the agent layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxPolicy {
    /// Default policy. Only controlled host functions and restricted network.
    WasmOnly,
    /// Allows workspace reads/writes but forbids arbitrary subprocesses.
    WorkspaceWrite,
    /// Executes inside a container with proxy-controlled networking.
    ContainerRestricted,
    /// Elevated container access; requires trusted extensions and admin approval.
    ContainerElevated,
}

/// Errors produced by the sandbox crate.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("container sandbox not yet implemented")]
    ContainerNotImplemented,

    #[error("network access denied for host {host}:{port}")]
    NetworkDenied { host: String, port: u16 },

    #[error("invalid WASM module: {0}")]
    InvalidModule(String),

    #[error("missing WASM export: {0}")]
    MissingExport(String),

    #[error("WASM execution timed out")]
    Timeout,

    #[error("WASM resource limit exceeded: {0}")]
    ResourceLimitExceeded(String),

    #[error("WASM execution failed: {0}")]
    WasmExecution(String),

    #[error("sandbox execution failed: {reason}")]
    ExecutionFailed { reason: String },
}
