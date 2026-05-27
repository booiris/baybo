use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub workspace_root: PathBuf,
    pub readable_paths: Vec<PathBuf>,
    /// Extra host paths the child may read **and** write through. The
    /// workspace bind is always RW; this list is for opt-in writes
    /// outside the workspace (e.g. a sandboxed script writing a
    /// rendered artefact into a project directory the agent owns). Each
    /// entry mounts at the same path inside the sandbox.
    #[serde(default)]
    pub writable_paths: Vec<PathBuf>,
    pub allowed_hosts: BTreeSet<String>,
    pub network_policy: NetworkPolicy,
    pub env: EnvPolicy,
    #[serde(skip)]
    pub stdin: StdinSource,
    pub timeout: Duration,
    #[serde(default)]
    pub resource_limits: ResourceLimits,
    #[serde(default)]
    pub filesystem_policy: FilesystemPolicy,
}

/// Sensitive host subpaths the permissive sandbox masks with a per-call
/// empty `tmpfs`. Targets credential vaults (SSH/GPG keys, cloud-CLI
/// tokens, Docker / kube auth) and the agent's own state directory
/// (libsql, identity files, secrets) — paths the agent has zero
/// legitimate need to read or write through a shell. Filesystem
/// permission bits already block most of these for an unprivileged
/// agent process; the tmpfs adds defence-in-depth so a misbehaving
/// command cannot even *enumerate* the directory contents.
///
/// `home` is the user's `$HOME` (caller resolves `HOME` / falls back
/// as appropriate); `aura_state` is `$AURA_HOME` or `~/.aura`.
/// Non-existent entries are filtered later at adapter-build time, so
/// it's safe to return paths that may not exist on every host.
pub fn default_sensitive_denylist(home: Option<&Path>, aura_state: Option<&Path>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(h) = home {
        out.push(h.join(".ssh"));
        out.push(h.join(".aws"));
        out.push(h.join(".gnupg"));
        out.push(h.join(".gpg"));
        out.push(h.join(".config").join("gh"));
        out.push(h.join(".config").join("gcloud"));
        out.push(h.join(".docker"));
        out.push(h.join(".kube"));
    }
    if let Some(s) = aura_state {
        out.push(s.to_path_buf());
    }
    out
}

/// Selects the filesystem-visibility model for a sandboxed call.
///
/// `Workspace` (default) is the historical "deny by default" model:
/// only `workspace_root`, `readable_paths`, and `writable_paths` are
/// visible. No tool ships this in production today; the variant is
/// retained for future per-call deny-by-default tools (e.g. an
/// LLM-generated script runner that wants blast-radius-zero).
///
/// `Permissive` widens that scope by *one extra RW root* — typically
/// the user's `$HOME` — alongside `workspace_root`. FHS roots
/// (`/usr`, `/bin`, `/sbin`, `/lib`, `/lib64`, `/etc`,
/// `/run/systemd/resolve`) remain RO-bound so installed binaries and
/// resolv.conf still work. Anything outside `workspace_root +
/// extra_root + FHS-RO` stays invisible — there is no full host-root
/// bind. Each entry in `denied_paths` is then masked with a per-call
/// empty `tmpfs` so credential vaults (`~/.ssh`, `~/.aws`, …) stay
/// unreadable even though they sit inside `extra_root`. The OS user's
/// own permission bits remain in effect on top.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FilesystemPolicy {
    #[default]
    Workspace,
    Permissive {
        /// Extra RW host root bound alongside `workspace_root`. The
        /// agent layer defaults this to `$HOME`.
        extra_root: PathBuf,
        denied_paths: Vec<PathBuf>,
    },
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
    /// Baseline env plus explicit `KEY=value` pairs that are NOT present in
    /// the parent process (e.g. secrets resolved from the vault for a Bash
    /// `secret_env`). Injected into the child via `--setenv` (bwrap) /
    /// `env` args (macOS); never sourced from the parent environment.
    BaselineWithExtra {
        extra: Vec<(String, String)>,
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
