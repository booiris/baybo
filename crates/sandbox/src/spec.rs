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
/// (sqlite, identity files, secrets) — paths the agent has zero
/// legitimate need to read or write through a shell. Filesystem
/// permission bits already block most of these for an unprivileged
/// agent process; the tmpfs adds defence-in-depth so a misbehaving
/// command cannot even *enumerate* the directory contents.
///
/// `home` is the user's `$HOME` (caller resolves `HOME` / falls back
/// as appropriate); `baybo_state` is `$BAYBO_HOME` or `~/.baybo`.
/// Non-existent entries are filtered later at adapter-build time, so
/// it's safe to return paths that may not exist on every host.
pub fn default_sensitive_denylist(home: Option<&Path>, baybo_state: Option<&Path>) -> Vec<PathBuf> {
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
    if let Some(s) = baybo_state {
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

/// Read-only system roots the Linux (bwrap) backend binds into every
/// sandbox. Single source of truth for the argv builder and
/// [`path_visibility`].
pub(crate) const LINUX_RO_SYSTEM_ROOTS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/etc",
    "/run/systemd/resolve",
];

/// Read-only system roots the macOS (sandbox-exec) backend allows.
/// Same role as [`LINUX_RO_SYSTEM_ROOTS`] for the SBPL profile.
pub(crate) const MACOS_RO_SYSTEM_ROOTS: &[&str] = &[
    "/usr",
    "/System",
    "/Library",
    "/private/var/db/dyld",
    "/private/etc",
    "/private/tmp",
    "/bin",
    "/sbin",
];

/// Scratch roots every backend exposes in `Permissive` mode. `/tmp` is
/// a host bind; `/proc` and `/dev` are synthesised, so a host file path
/// underneath them is not a meaningful question — all three are treated
/// as "may be visible" so [`path_visibility`] never claims otherwise.
const SHARED_SCRATCH_ROOTS: &[&str] = &["/tmp", "/proc", "/dev"];

/// Whether `path` is reachable from inside a sandbox built with this
/// policy, as a three-valued answer:
///
/// - `Some(true)` — reachable on **every** backend.
/// - `Some(false)` — reachable on **no** backend.
/// - `None` — backend-dependent, or the question is not well-formed
///   (relative path, `Workspace` policy whose scratch roots are
///   per-call tmpfs). Callers MUST treat `None` as "do not claim
///   anything"; it is not a synonym for either boolean.
///
/// The system-root check deliberately unions the Linux and macOS lists
/// rather than picking the running platform's: a wrong `Some(false)`
/// would have the agent told a visible path is invisible, which is a
/// worse failure than declining to answer.
pub fn path_visibility(
    policy: &FilesystemPolicy,
    workspace_root: &Path,
    readable_paths: &[PathBuf],
    path: &Path,
) -> Option<bool> {
    if !path.is_absolute() {
        return None;
    }
    // Mount order is last-wins, so the re-exposing binds are tested
    // before the masking tmpfs: `readable_paths` and `workspace_root`
    // are bound *after* `denied_paths` precisely so a work dir nested
    // inside a masked state dir stays reachable.
    if path.starts_with(workspace_root) || readable_paths.iter().any(|p| path.starts_with(p)) {
        return Some(true);
    }
    let FilesystemPolicy::Permissive {
        extra_root,
        denied_paths,
    } = policy
    else {
        // `Workspace` mode gives `/tmp` a fresh per-call tmpfs and no
        // extra root; nothing outside the binds above is worth a claim.
        return None;
    };
    if denied_paths.iter().any(|p| path.starts_with(p)) {
        return Some(false);
    }
    if path.starts_with(extra_root) {
        return Some(true);
    }
    let under_any = |roots: &[&str]| roots.iter().any(|r| path.starts_with(Path::new(r)));
    if under_any(SHARED_SCRATCH_ROOTS) {
        return None;
    }
    if under_any(LINUX_RO_SYSTEM_ROOTS) || under_any(MACOS_RO_SYSTEM_ROOTS) {
        return None;
    }
    Some(false)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn permissive(extra_root: &str, denied: &[&str]) -> FilesystemPolicy {
        FilesystemPolicy::Permissive {
            extra_root: PathBuf::from(extra_root),
            denied_paths: denied.iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn a_path_outside_every_root_is_provably_invisible() {
        // The incident: the gateway binary lives in the repo's `target/`,
        // which no bind covers, so `exit 127` there means "never mounted".
        let policy = permissive("/home/u", &["/home/u/.baybo"]);
        assert_eq!(
            path_visibility(
                &policy,
                Path::new("/home/u/.baybo/work"),
                &[],
                Path::new("/data/aura/target/release/baybo")
            ),
            Some(false)
        );
    }

    #[test]
    fn the_re_exposing_binds_win_over_the_masking_tmpfs() {
        // Mount order is last-wins: the work dir and the readable skill tree
        // are bound AFTER the denylist tmpfs precisely so they survive it.
        let policy = permissive("/home/u", &["/home/u/.baybo"]);
        let work = Path::new("/home/u/.baybo/work");
        let skills = [PathBuf::from("/home/u/.baybo/personas/b/skills")];
        assert_eq!(
            path_visibility(
                &policy,
                work,
                &skills,
                Path::new("/home/u/.baybo/work/x.py")
            ),
            Some(true)
        );
        assert_eq!(
            path_visibility(
                &policy,
                work,
                &skills,
                Path::new("/home/u/.baybo/personas/b/skills/s/run.sh")
            ),
            Some(true)
        );
        // The rest of the masked tree stays hidden.
        assert_eq!(
            path_visibility(
                &policy,
                work,
                &skills,
                Path::new("/home/u/.baybo/state/storage.db")
            ),
            Some(false)
        );
    }

    #[test]
    fn the_extra_root_is_visible_and_system_roots_are_undecided() {
        let policy = permissive("/home/u", &["/home/u/.ssh"]);
        let work = Path::new("/home/u/.baybo/work");
        assert_eq!(
            path_visibility(&policy, work, &[], Path::new("/home/u/notes.txt")),
            Some(true)
        );
        assert_eq!(
            path_visibility(&policy, work, &[], Path::new("/home/u/.ssh/id_rsa")),
            Some(false)
        );
        // Backend-dependent roots must never be claimed either way: a wrong
        // `Some(false)` would tell the agent a mounted path is invisible.
        for p in [
            "/usr/bin/python3",
            "/tmp/scratch",
            "/private/tmp/x",
            "/System/L",
        ] {
            assert_eq!(
                path_visibility(&policy, work, &[], Path::new(p)),
                None,
                "{p}"
            );
        }
    }

    #[test]
    fn relative_paths_and_workspace_mode_decline_to_answer() {
        let policy = permissive("/home/u", &[]);
        assert_eq!(
            path_visibility(&policy, Path::new("/home/u/w"), &[], Path::new("target/x")),
            None
        );
        // `Workspace` mode gives `/tmp` a per-call tmpfs and has no extra
        // root; only the explicit binds are worth a claim.
        assert_eq!(
            path_visibility(
                &FilesystemPolicy::Workspace,
                Path::new("/home/u/w"),
                &[],
                Path::new("/home/u/w/x")
            ),
            Some(true)
        );
        assert_eq!(
            path_visibility(
                &FilesystemPolicy::Workspace,
                Path::new("/home/u/w"),
                &[],
                Path::new("/opt/x")
            ),
            None
        );
    }
}
