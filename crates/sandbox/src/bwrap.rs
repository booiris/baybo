#![cfg(all(target_os = "linux", feature = "linux"))]

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Instant;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::SandboxRunner;
use crate::args::build_bwrap_argv;
use crate::bootstrap::{SandboxAvailability, locate_binary, parse_version};
use crate::error::SandboxError;
use crate::spec::{Backend, ResourceLimits, SandboxOutput, SandboxSpec, StdinSource};

pub struct BwrapRunner {
    binary: PathBuf,
    /// `systemd-run` binary path when the host actually has a working
    /// user systemd manager; `None` means we run bwrap directly without
    /// cgroup-backed resource caps. Probed once at `discover()` and
    /// cached for the runner's lifetime — operators see the result in
    /// the startup log line.
    systemd_run: Option<PathBuf>,
}

impl BwrapRunner {
    pub fn discover() -> Result<Self, SandboxError> {
        let binary = locate_binary("bwrap")?;
        let systemd_run = probe_systemd_run();
        match &systemd_run {
            Some(p) => tracing::info!(
                path = %p.display(),
                "systemd-run available; bwrap calls will run inside transient user scopes"
            ),
            None => tracing::warn!(
                "systemd-run unavailable; bwrap calls will run without cgroup memory/pid caps"
            ),
        }
        Ok(Self {
            binary,
            systemd_run,
        })
    }

    pub async fn probe() -> Result<SandboxAvailability, SandboxError> {
        let binary = locate_binary("bwrap")?;
        let out = Command::new(&binary).arg("--version").output().await?;
        Ok(SandboxAvailability {
            backend: Backend::Bwrap,
            binary_path: binary,
            version: parse_version(&out.stdout),
        })
    }

    /// Returns whether the runner is wrapping bwrap with `systemd-run`
    /// for cgroup-backed resource caps. Used by tests; operators get
    /// the same information from the startup log.
    pub fn has_resource_cap_support(&self) -> bool {
        self.systemd_run.is_some()
    }
}

/// Whether the host has a usable `systemd-run --user`. The probe
/// actually executes `/bin/true` inside a transient user scope rather
/// than just checking that the binary exists, because most "broken"
/// configurations (no user manager, dbus session unreachable, missing
/// XDG_RUNTIME_DIR) still ship the binary and only fail at the unit
/// activation step.
fn probe_systemd_run() -> Option<PathBuf> {
    let bin = locate_binary("systemd-run").ok()?;
    let out = std::process::Command::new(&bin)
        .args(["--user", "--quiet", "--scope", "--", "/bin/true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    out.success().then_some(bin)
}

/// Pure validation of a `SandboxSpec` against bwrap's enforcement
/// capabilities. Lifted out of `BwrapRunner::run` so unit tests can
/// exercise the fail-closed paths without spawning bwrap, and so the
/// rule lives next to `build_bwrap_argv` for ease of audit.
///
/// `has_systemd_run` reflects what `BwrapRunner::discover()` cached
/// at startup: whether `systemd-run --user --scope -- /bin/true`
/// actually worked on this host. Without it, the runner has no
/// kernel-level mechanism to apply cgroup memory/pid caps and a
/// non-`unlimited()` `resource_limits` is unenforceable.
///
/// Per-host network allowlists (`allowed_hosts`) are deliberately
/// *not* validated here. bwrap with `--share-net` shares the host's
/// network namespace and there is no in-tree primitive that can stop
/// a direct `connect()`, so the field would be advisory at best — but
/// shipping that as fail-closed in this round would conflate the
/// "resource caps" hardening with a separate per-host enforcement
/// design (see `docs/todo/sandbox-os-isolation.md` for the future
/// netns-based path). The runner logs a one-line warn at run() so
/// operators see the limitation without the call hard-failing.
pub(crate) fn validate_spec(spec: &SandboxSpec, has_systemd_run: bool) -> Result<(), SandboxError> {
    if !spec.resource_limits.is_unlimited() && !has_systemd_run {
        return Err(SandboxError::Unenforceable {
            backend: "bwrap",
            what: format!(
                "resource_limits {:?} (no working `systemd-run --user`)",
                spec.resource_limits
            ),
            hint: "install systemd user manager, run baybo under a systemd user unit, or pass `ResourceLimits::unlimited()`",
        });
    }
    Ok(())
}

/// Build the `systemd-run --user --scope` prefix that wraps a bwrap
/// invocation in a transient cgroup. Returned argv lacks the trailing
/// program/args — the caller appends `bwrap_path bwrap_argv...` after.
pub(crate) fn build_systemd_run_prefix(limits: &ResourceLimits) -> Vec<OsString> {
    let mut argv: Vec<OsString> = Vec::with_capacity(8);
    argv.push(OsString::from("--user"));
    argv.push(OsString::from("--quiet"));
    argv.push(OsString::from("--scope"));
    // Auto-deallocate the transient scope unit on exit so failed
    // invocations don't leave dangling units in the user manager's
    // state.
    argv.push(OsString::from("--collect"));
    if let Some(bytes) = limits.memory_max_bytes {
        argv.push(OsString::from(format!("--property=MemoryMax={bytes}")));
    }
    if let Some(pids) = limits.pids_max {
        argv.push(OsString::from(format!("--property=TasksMax={pids}")));
    }
    argv.push(OsString::from("--"));
    argv
}

impl BwrapRunner {
    /// Build the (unspawned) `Command` for a spec — shared by the blocking
    /// [`SandboxRunner::run`] and the detached [`SandboxRunner::spawn_detached`].
    fn build_command(&self, spec: &SandboxSpec) -> Command {
        let workspace_symlink_mount = spec
            .cwd
            .as_deref()
            .and_then(|cwd| crate::workspace_symlink_mount_for(cwd, &spec.workspace_root));
        let bwrap_argv = build_bwrap_argv(spec, workspace_symlink_mount.as_ref());

        let limits_set = spec.resource_limits.memory_max_bytes.is_some()
            || spec.resource_limits.pids_max.is_some();
        let mut cmd = match (&self.systemd_run, limits_set) {
            (Some(systemd_run), true) => {
                let mut c = Command::new(systemd_run);
                let prefix = build_systemd_run_prefix(&spec.resource_limits);
                c.args(&prefix);
                c.arg(&self.binary);
                c.args(&bwrap_argv);
                c
            }
            _ => {
                let mut c = Command::new(&self.binary);
                c.args(&bwrap_argv);
                c
            }
        };
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd.stdin(match spec.stdin {
            StdinSource::Null => Stdio::null(),
            StdinSource::Inherit => Stdio::inherit(),
            StdinSource::Bytes(_) | StdinSource::Piped => Stdio::piped(),
        });
        cmd
    }

    /// Validate + spawn the child, wiring `Bytes` stdin via a writer task.
    /// The shared spawn path for `run` and `spawn_detached`.
    fn spawn_child(&self, spec: &SandboxSpec) -> Result<tokio::process::Child, SandboxError> {
        validate_spec(spec, self.systemd_run.is_some())?;
        let mut cmd = self.build_command(spec);
        let mut child = cmd.spawn()?;
        if let StdinSource::Bytes(bytes) = &spec.stdin
            && let Some(mut stdin) = child.stdin.take()
        {
            let bytes = bytes.clone();
            tokio::spawn(async move {
                let _ = stdin.write_all(&bytes).await;
            });
        }
        Ok(child)
    }
}

#[async_trait]
impl SandboxRunner for BwrapRunner {
    async fn run(&self, spec: SandboxSpec) -> Result<SandboxOutput, SandboxError> {
        if matches!(spec.stdin, StdinSource::Piped) {
            return Err(SandboxError::InvalidSpec(
                "StdinSource::Piped requires spawn_detached".into(),
            ));
        }
        if !spec.allowed_hosts.is_empty() {
            tracing::warn!(
                hosts = ?spec.allowed_hosts,
                "bwrap backend ignores allowed_hosts; egress is still all-or-nothing per network_policy. Per-host enforcement is deferred — see docs/todo/sandbox-os-isolation.md"
            );
        }
        let started = Instant::now();
        let child = self.spawn_child(&spec)?;
        let wait = child.wait_with_output();
        let result = tokio::time::timeout(spec.timeout, wait).await;
        let elapsed = started.elapsed();

        match result {
            Ok(Ok(out)) => {
                let exit_code = out.status.code().unwrap_or(-1);
                let stderr_str = String::from_utf8_lossy(&out.stderr);
                if !out.status.success() && stderr_str.starts_with("bwrap: ") {
                    return Err(SandboxError::BackendFailure {
                        status: out.status.code(),
                        stderr: stderr_str.into_owned(),
                    });
                }
                Ok(SandboxOutput {
                    exit_code,
                    stdout: out.stdout,
                    stderr: out.stderr,
                    elapsed,
                    timed_out: false,
                })
            }
            Ok(Err(e)) => Err(SandboxError::Io(e)),
            Err(_) => Err(SandboxError::Timeout(spec.timeout)),
        }
    }

    async fn spawn_detached(
        &self,
        spec: SandboxSpec,
    ) -> Result<Box<dyn crate::DetachedChild>, SandboxError> {
        if !spec.allowed_hosts.is_empty() {
            tracing::warn!(
                hosts = ?spec.allowed_hosts,
                "bwrap backend ignores allowed_hosts; egress is still all-or-nothing per network_policy."
            );
        }
        // No internal timeout: the caller owns the foreground wait + the
        // detached escort. `kill_on_drop` + the child's process group still
        // bound it on shutdown.
        let child = self.spawn_child(&spec)?;
        Ok(Box::new(crate::TokioDetachedChild(child)))
    }

    fn backend(&self) -> Backend {
        Backend::Bwrap
    }

    fn default_resource_limits(&self) -> ResourceLimits {
        // Without `systemd-run --user` the runner has no way to
        // enforce cgroup caps. Returning `safe_defaults()` from this
        // method would feed `validate_spec` an Unenforceable spec on
        // every default-policy call.
        if self.systemd_run.is_some() {
            ResourceLimits::safe_defaults()
        } else {
            ResourceLimits::unlimited()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{EnvPolicy, NetworkPolicy, StdinSource};
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::time::Duration;

    fn argv_strs(argv: &[OsString]) -> Vec<String> {
        argv.iter().map(|s| s.to_string_lossy().into()).collect()
    }

    fn baseline_spec() -> SandboxSpec {
        SandboxSpec {
            program: PathBuf::from("/bin/true"),
            args: vec![],
            cwd: None,
            workspace_root: PathBuf::from("/tmp/ws"),
            readable_paths: vec![],
            writable_paths: vec![],
            allowed_hosts: BTreeSet::new(),
            network_policy: NetworkPolicy::All,
            env: EnvPolicy::Baseline,
            stdin: StdinSource::Null,
            timeout: Duration::from_secs(5),
            resource_limits: ResourceLimits::unlimited(),
            filesystem_policy: crate::spec::FilesystemPolicy::default(),
        }
    }

    #[test]
    fn validate_spec_accepts_non_empty_allowed_hosts() {
        // bwrap deliberately does NOT fail-close on allowed_hosts in
        // this round — the field is advisory pending the netns+nft
        // work documented in docs/todo/sandbox-os-isolation.md. The
        // runner logs a warn at runtime; validation just returns Ok.
        let mut spec = baseline_spec();
        spec.allowed_hosts = BTreeSet::from(["github.com:443".to_string()]);
        validate_spec(&spec, true).expect("allowed_hosts must not fail-close on bwrap");
    }

    #[test]
    fn validate_spec_rejects_resource_limits_without_systemd_run() {
        let mut spec = baseline_spec();
        spec.resource_limits = ResourceLimits::safe_defaults();
        let err = validate_spec(&spec, false).expect_err("must refuse");
        let SandboxError::Unenforceable { backend, what, .. } = err else {
            panic!("expected Unenforceable variant");
        };
        assert_eq!(backend, "bwrap");
        assert!(
            what.contains("resource_limits"),
            "expected resource_limits in error: {what}"
        );
    }

    #[test]
    fn validate_spec_accepts_unlimited_without_systemd_run() {
        let spec = baseline_spec();
        validate_spec(&spec, false).expect("unlimited limits must pass without systemd-run");
    }

    #[test]
    fn validate_spec_accepts_safe_defaults_with_systemd_run() {
        let mut spec = baseline_spec();
        spec.resource_limits = ResourceLimits::safe_defaults();
        validate_spec(&spec, true).expect("safe defaults must pass with systemd-run");
    }

    #[test]
    fn systemd_run_prefix_carries_user_scope_collect_and_terminator() {
        let argv = build_systemd_run_prefix(&ResourceLimits::unlimited());
        let strs = argv_strs(&argv);
        assert!(strs.contains(&"--user".to_string()));
        assert!(strs.contains(&"--quiet".to_string()));
        assert!(strs.contains(&"--scope".to_string()));
        assert!(strs.contains(&"--collect".to_string()));
        assert_eq!(strs.last().map(String::as_str), Some("--"));
    }

    #[test]
    fn systemd_run_prefix_emits_memory_and_pids_properties() {
        let limits = ResourceLimits {
            memory_max_bytes: Some(256 * 1024 * 1024),
            pids_max: Some(64),
        };
        let argv = build_systemd_run_prefix(&limits);
        let strs = argv_strs(&argv);
        assert!(
            strs.iter()
                .any(|a| a == &format!("--property=MemoryMax={}", 256u64 * 1024 * 1024))
        );
        assert!(strs.iter().any(|a| a == "--property=TasksMax=64"));
    }

    #[test]
    fn systemd_run_prefix_omits_properties_when_unlimited() {
        let argv = build_systemd_run_prefix(&ResourceLimits::unlimited());
        let strs = argv_strs(&argv);
        assert!(
            !strs.iter().any(|a| a.starts_with("--property=MemoryMax")),
            "unlimited memory must not emit MemoryMax: {strs:?}"
        );
        assert!(
            !strs.iter().any(|a| a.starts_with("--property=TasksMax")),
            "unlimited pids must not emit TasksMax: {strs:?}"
        );
    }
}
