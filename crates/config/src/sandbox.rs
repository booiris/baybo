//! Configuration for how the agent's `ExecCommand` tools (today: `Bash`) are
//! isolated. The default — and the only mode a normal build honors — is the
//! OS sandbox ([`SandboxMode::Sandboxed`]).

use serde::{Deserialize, Serialize};

/// How shell-out tools are isolated.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    /// Wrap every shell command in the platform OS sandbox (bwrap / sandbox-exec
    /// / docker). The normal, secure default.
    #[default]
    Sandboxed,
    /// Run shell commands directly — no OS isolation, no work-dir path jail,
    /// cwd inherited from the process. **Dangerous:** only meaningful when aura
    /// itself already runs inside a disposable, isolated environment (a
    /// benchmark task container) where the OS sandbox is both unavailable and
    /// redundant. A build that wasn't compiled with the `bench-passthrough`
    /// feature refuses to start when this is selected, rather than silently
    /// downgrading isolation.
    Passthrough,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct SandboxConfig {
    pub mode: SandboxMode,
}
