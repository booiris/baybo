use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub workspace_root: PathBuf,
    pub readable_paths: Vec<PathBuf>,
    pub allowed_hosts: BTreeSet<String>,
    pub network_policy: NetworkPolicy,
    pub env: EnvPolicy,
    #[serde(skip)]
    pub stdin: StdinSource,
    pub timeout: Duration,
    #[serde(default)]
    pub resource_limits: ResourceLimits,
}

/// Per-invocation resource caps applied by whichever isolation backend
/// supports them. `None` keeps the backend default (which on Linux means
/// "inherit the host's per-user limits", on Docker means "the daemon's
/// defaults" — i.e. essentially unlimited from the agent's perspective).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// Hard memory ceiling in bytes. Backed by cgroup `memory.max` on
    /// Linux (via `systemd-run`) and `--memory` on Docker. macOS has no
    /// SBPL equivalent and ignores the value.
    pub memory_max_bytes: Option<u64>,
    /// Maximum number of processes/threads. Backed by cgroup
    /// `pids.max` (Linux) and `--pids-limit` (Docker). macOS ignores.
    pub pids_max: Option<u64>,
}

impl ResourceLimits {
    pub const fn unlimited() -> Self {
        Self {
            memory_max_bytes: None,
            pids_max: None,
        }
    }

    /// Conservative caps sized to absorb typical agent-issued shell
    /// snippets (build/test invocations, small scripts) without
    /// giving a runaway tool the whole machine. Used by runners that
    /// can actually enforce these (Docker; bwrap when systemd-run is
    /// available); see `SandboxRunner::default_resource_limits`.
    pub const fn safe_defaults() -> Self {
        Self {
            memory_max_bytes: Some(512 * 1024 * 1024),
            pids_max: Some(256),
        }
    }

    pub const fn is_unlimited(&self) -> bool {
        self.memory_max_bytes.is_none() && self.pids_max.is_none()
    }
}

impl Default for ResourceLimits {
    /// Library default is permissive (unlimited). Callers who want
    /// caps should either set them explicitly via the builder methods
    /// or accept whatever the runner picks via
    /// `SandboxRunner::default_resource_limits`. The default is NOT
    /// `safe_defaults()` because some backends (sandbox-exec, bwrap
    /// without `systemd-run`) cannot enforce those caps; defaulting
    /// to unenforceable values would push every call site into either
    /// constant fail-closed errors or a per-backend opt-out dance.
    fn default() -> Self {
        Self::unlimited()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    None,
    All,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EnvPolicy {
    #[default]
    Baseline,
    Allowlist {
        vars: Vec<String>,
    },
}

#[derive(Debug, Clone, Default)]
pub enum StdinSource {
    #[default]
    Null,
    Inherit,
    Bytes(Vec<u8>),
}

#[derive(Debug)]
pub struct SandboxOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub elapsed: Duration,
    pub timed_out: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Bwrap,
    SandboxExec,
    Docker,
}
