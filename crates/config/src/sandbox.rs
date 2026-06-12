//! Configuration for how the agent's `ExecCommand` tools (today: `Bash`) are
//! isolated. The default is [`SandboxMode::Auto`] (OS sandbox plus an on-failure
//! LLM risk judge); [`SandboxMode::Sandboxed`] is the stricter, judge-free mode.

use serde::{Deserialize, Serialize};

/// How shell-out tools are isolated.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    /// Wrap every shell command in the platform OS sandbox (bwrap / sandbox-exec
    /// / docker), with no risk judge: a sandboxed failure is returned as-is and
    /// destructive commands fall back to the static token approval gate. The
    /// strictest mode — pick it to keep the agent fully inside the sandbox.
    Sandboxed,
    /// Sandboxed execution plus an on-failure LLM risk judge, and the **default**:
    /// when a sandboxed command fails (non-zero exit, or the destructive-token
    /// pre-check), a model judges whether running it outside the sandbox is safe
    /// and whether the failure was the sandbox's fault. Safe + sandbox-related
    /// runs are re-executed unsandboxed automatically; risky ones prompt the user
    /// (or, in an unattended session, return the original error). Degrades to
    /// `Sandboxed` when no LLM is available. A production mode — no special build
    /// feature required.
    #[default]
    Auto,
    /// No OS sandbox at all — run shell commands directly via `sh -c`, with no
    /// work-dir path jail and cwd inherited from the process. **Dangerous:** the
    /// agent has the full reach of the host user. Only meaningful when aura
    /// itself already runs inside a disposable, isolated environment (a
    /// benchmark task container, a throwaway VM) where the OS sandbox is both
    /// unavailable — bwrap can't nest — and redundant. Pick it deliberately;
    /// there is no isolation fallback.
    None,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct SandboxConfig {
    pub mode: SandboxMode,
}
