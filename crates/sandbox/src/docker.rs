#![cfg(feature = "docker")]

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::SandboxRunner;
use crate::args::build_docker_argv;
use crate::bootstrap::{SandboxAvailability, locate_binary, parse_version};
use crate::error::SandboxError;
use crate::spec::{Backend, ResourceLimits, SandboxOutput, SandboxSpec, StdinSource};

/// Default container image used when no override is configured.
///
/// `debian:stable-slim` gives us GNU coreutils + a real `sh`/`bash`,
/// which most agent-issued shell snippets expect. Alpine's busybox
/// shell is smaller but diverges on flag handling.
///
/// The runner pulls this tag once at startup (`warm()`) and stores
/// the resolved digest reference (`debian@sha256:…`) — every
/// subsequent `docker run` uses the digest with `--pull=never`, so
/// upstream rotations of the floating tag cannot silently swap the
/// trusted execution base.
pub const DEFAULT_IMAGE: &str = "debian:stable-slim";

/// Cross-platform sandbox backend that delegates isolation to the
/// docker daemon. Selected by `current_platform_runner()` when the
/// native backend (bwrap on Linux, sandbox-exec on macOS) is missing.
pub struct DockerRunner {
    binary: PathBuf,
    image: String,
    /// Set by `warm()`. When present, every `run()` uses this digest
    /// reference so the trust boundary is fixed for the lifetime of
    /// the gateway process.
    pinned_image: OnceLock<String>,
}

impl DockerRunner {
    /// Verify the docker CLI is on PATH and the daemon is reachable.
    ///
    /// The daemon check matters because a host can have the docker
    /// binary installed without dockerd running (or without the user
    /// having permission to talk to its socket). Without it, every
    /// ExecCommand call would fail at the first `docker run` instead
    /// of at startup.
    pub fn discover() -> Result<Self, SandboxError> {
        let binary = locate_binary("docker")?;
        let probe = std::process::Command::new(&binary)
            .args(["info", "--format", "{{.ServerVersion}}"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(SandboxError::Io)?;
        if !probe.status.success() {
            let message = String::from_utf8_lossy(&probe.stderr).trim().to_string();
            return Err(SandboxError::BackendUnreachable {
                name: "docker",
                message: if message.is_empty() {
                    "docker info exited non-zero".into()
                } else {
                    message
                },
            });
        }
        Ok(Self {
            binary,
            image: DEFAULT_IMAGE.into(),
            pinned_image: OnceLock::new(),
        })
    }

    pub fn with_image(mut self, image: impl Into<String>) -> Self {
        self.image = image.into();
        self.pinned_image = OnceLock::new();
        self
    }

    pub async fn probe() -> Result<SandboxAvailability, SandboxError> {
        let binary = locate_binary("docker")?;
        let out = Command::new(&binary).arg("--version").output().await?;
        Ok(SandboxAvailability {
            backend: Backend::Docker,
            binary_path: binary,
            version: parse_version(&out.stdout),
        })
    }

    fn effective_image(&self) -> &str {
        self.pinned_image
            .get()
            .map(String::as_str)
            .unwrap_or(self.image.as_str())
    }

    /// Resolve the configured image to its digest reference, pulling
    /// it from the registry first if it is not already present
    /// locally. Idempotent — the second call is a no-op.
    async fn pin_image(&self) -> Result<(), SandboxError> {
        if self.pinned_image.get().is_some() {
            return Ok(());
        }
        if !self.image_is_local().await? {
            self.pull_image().await?;
        }
        let digest_ref = self.resolve_digest_ref().await?;
        // First writer wins; ignore Err on race.
        let _ = self.pinned_image.set(digest_ref);
        Ok(())
    }

    async fn image_is_local(&self) -> Result<bool, SandboxError> {
        let out = Command::new(&self.binary)
            .args([
                "image",
                "inspect",
                "--format",
                "{{.Id}}",
                self.image.as_str(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map_err(SandboxError::Io)?;
        Ok(out.success())
    }

    async fn pull_image(&self) -> Result<(), SandboxError> {
        tracing::info!(image = %self.image, "pulling sandbox container image (one-time per gateway boot)");
        let out = Command::new(&self.binary)
            .args(["pull", self.image.as_str()])
            .output()
            .await
            .map_err(SandboxError::Io)?;
        if !out.status.success() {
            return Err(SandboxError::BackendFailure {
                status: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        Ok(())
    }

    async fn resolve_digest_ref(&self) -> Result<String, SandboxError> {
        // `RepoDigests` is a list like `["debian@sha256:abc…"]`. We
        // use the first entry — there is exactly one for an image
        // pulled from a single registry, and any of them is a valid
        // digest reference for `docker run`.
        let out = Command::new(&self.binary)
            .args([
                "image",
                "inspect",
                "--format",
                "{{if .RepoDigests}}{{index .RepoDigests 0}}{{end}}",
                self.image.as_str(),
            ])
            .output()
            .await
            .map_err(SandboxError::Io)?;
        if !out.status.success() {
            return Err(SandboxError::BackendFailure {
                status: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        let digest = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if digest.is_empty() {
            // Locally-built images have no RepoDigest. Fall back to
            // the original tag — `--pull=never` still prevents drift
            // for the gateway lifetime, just without crypto pinning.
            tracing::warn!(
                image = %self.image,
                "image has no RepoDigest; sandbox runs will use the floating tag with --pull=never",
            );
            Ok(self.image.clone())
        } else {
            tracing::info!(image = %self.image, digest = %digest, "pinned sandbox container image");
            Ok(digest)
        }
    }
}

/// RAII guard that force-removes a daemon-side container on drop
/// unless explicitly disarmed. The local docker CLI is killed by
/// `kill_on_drop`, but dockerd-managed containers live independently
/// of our process tree — without this, an outer timeout or upstream
/// cancellation that drops `run()`'s future leaves the workload
/// running and still writing into the workspace bind.
struct ContainerCleanupGuard {
    binary: PathBuf,
    name: String,
    armed: bool,
}

impl ContainerCleanupGuard {
    fn new(binary: PathBuf, name: String) -> Self {
        Self {
            binary,
            name,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ContainerCleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Detach onto a std thread: Drop may run on a tokio runtime
        // worker (the future was cancelled mid-await), and we don't
        // want to block that worker on a docker rm subprocess. The
        // call is fire-and-forget by design — best-effort cleanup
        // and any failure is just logged via stderr being /dev/null.
        let binary = std::mem::take(&mut self.binary);
        let name = std::mem::take(&mut self.name);
        let _ = std::thread::Builder::new()
            .name("baybo-sandbox-docker-cleanup".into())
            .spawn(move || {
                let _ = std::process::Command::new(&binary)
                    .args(["rm", "-f", &name])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            });
    }
}

#[async_trait]
impl SandboxRunner for DockerRunner {
    async fn warm(&self) -> Result<(), SandboxError> {
        self.pin_image().await
    }

    async fn run(&self, spec: SandboxSpec) -> Result<SandboxOutput, SandboxError> {
        if matches!(spec.stdin, StdinSource::Piped) {
            return Err(SandboxError::InvalidSpec(
                "StdinSource::Piped is not supported by the docker backend".into(),
            ));
        }
        if !spec.allowed_hosts.is_empty() {
            // Docker has no in-tree per-host enforcement. The proper
            // kernel-level filter is scoped in
            // docs/todo/sandbox-os-isolation.md but deferred. This
            // round is scoped to resource caps only on Linux/Docker,
            // so the field is advisory: warn so operators see it,
            // then run with the regular bridge network.
            tracing::warn!(
                hosts = ?spec.allowed_hosts,
                "docker backend ignores allowed_hosts; egress is still all-or-nothing per network_policy. Per-host enforcement is deferred — see docs/todo/sandbox-os-isolation.md"
            );
        }
        let workspace_symlink_mount = spec
            .cwd
            .as_deref()
            .and_then(|cwd| crate::workspace_symlink_mount_for(cwd, &spec.workspace_root));
        let container_name = unique_container_name();
        let argv = build_docker_argv(
            &spec,
            workspace_symlink_mount.as_ref(),
            self.effective_image(),
            &container_name,
        );
        let mut cmd = Command::new(&self.binary);
        cmd.args(&argv)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // `Piped` is refused up front on this backend (docker would need
        // `docker run -i` plus attach plumbing); the arm exists only for
        // match exhaustiveness.
        cmd.stdin(match spec.stdin {
            StdinSource::Null | StdinSource::Piped => Stdio::null(),
            StdinSource::Inherit => Stdio::inherit(),
            StdinSource::Bytes(_) => Stdio::piped(),
        });

        // Arm the cleanup guard before spawning. From this point on,
        // every exit path — including the future being dropped by an
        // outer timeout or a select! cancellation upstream — fires a
        // `docker rm -f` against the daemon. Disarmed only on the
        // success path where `--rm` already removed the container.
        let mut cleanup = ContainerCleanupGuard::new(self.binary.clone(), container_name.clone());

        let started = Instant::now();
        let mut child = cmd.spawn()?;

        if let StdinSource::Bytes(bytes) = &spec.stdin
            && let Some(mut stdin) = child.stdin.take()
        {
            let bytes = bytes.clone();
            tokio::spawn(async move {
                let _ = stdin.write_all(&bytes).await;
            });
        }

        let wait = child.wait_with_output();
        let result = tokio::time::timeout(spec.timeout, wait).await;
        let elapsed = started.elapsed();

        match result {
            Ok(Ok(out)) => {
                let exit_code = out.status.code().unwrap_or(-1);
                // Heuristic: docker exits 125 for daemon-side launch
                // failures (e.g. socket unavailable, missing image).
                // Surface that as a backend-setup error so the agent
                // doesn't mis-attribute it to the user command. The
                // cleanup guard stays armed because a partially-
                // created container may still be sitting in the
                // daemon's stopped state.
                if !out.status.success() && exit_code == 125 && !out.stderr.is_empty() {
                    return Err(SandboxError::BackendFailure {
                        status: out.status.code(),
                        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                    });
                }
                // Clean docker-CLI exit means `--rm` removed the
                // container; skip the redundant cleanup.
                cleanup.disarm();
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
        if matches!(spec.stdin, StdinSource::Piped) {
            return Err(SandboxError::InvalidSpec(
                "StdinSource::Piped is not supported by the docker backend".into(),
            ));
        }
        if !spec.allowed_hosts.is_empty() {
            tracing::warn!(
                hosts = ?spec.allowed_hosts,
                "docker backend ignores allowed_hosts; egress is still all-or-nothing per network_policy."
            );
        }
        let workspace_symlink_mount = spec
            .cwd
            .as_deref()
            .and_then(|cwd| crate::workspace_symlink_mount_for(cwd, &spec.workspace_root));
        let container_name = unique_container_name();
        let argv = build_docker_argv(
            &spec,
            workspace_symlink_mount.as_ref(),
            self.effective_image(),
            &container_name,
        );
        let mut cmd = Command::new(&self.binary);
        cmd.args(&argv)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // `Piped` is refused up front on this backend (docker would need
        // `docker run -i` plus attach plumbing); the arm exists only for
        // match exhaustiveness.
        cmd.stdin(match spec.stdin {
            StdinSource::Null | StdinSource::Piped => Stdio::null(),
            StdinSource::Inherit => Stdio::inherit(),
            StdinSource::Bytes(_) => Stdio::piped(),
        });
        // The cleanup guard removes the container on drop (process shutdown
        // without an explicit `wait`). `DockerDetachedChild::wait` disarms it
        // once the container is gone — removed by `--rm` on a clean exit, or by
        // an explicit awaited `docker rm -f` when `start_kill` SIGKILLed the
        // client (which would otherwise orphan the daemon-side container).
        let cleanup = ContainerCleanupGuard::new(self.binary.clone(), container_name.clone());
        let mut child = cmd.spawn()?;
        if let StdinSource::Bytes(bytes) = &spec.stdin
            && let Some(mut stdin) = child.stdin.take()
        {
            let bytes = bytes.clone();
            tokio::spawn(async move {
                let _ = stdin.write_all(&bytes).await;
            });
        }
        Ok(Box::new(DockerDetachedChild {
            child,
            binary: self.binary.clone(),
            container_name,
            cleanup,
            kill_requested: false,
        }))
    }

    fn backend(&self) -> Backend {
        Backend::Docker
    }

    fn default_resource_limits(&self) -> ResourceLimits {
        // dockerd always honors `--memory` / `--pids-limit`, so the
        // safe defaults are always enforceable here.
        ResourceLimits::safe_defaults()
    }
}

/// A detached `docker run` child. Killing it must remove the container by
/// name (SIGKILL of the `docker run` client orphans it); on a clean exit
/// `--rm` already removed it, so the drop guard is disarmed.
struct DockerDetachedChild {
    child: tokio::process::Child,
    binary: PathBuf,
    container_name: String,
    cleanup: ContainerCleanupGuard,
    /// Set by `start_kill`: the `docker run` client was SIGKILLed, which
    /// orphans the daemon-side container, so `wait` must `docker rm -f` it.
    kill_requested: bool,
}

#[async_trait]
impl crate::DetachedChild for DockerDetachedChild {
    fn take_stdout(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
        self.child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>)
    }
    fn take_stderr(&mut self) -> Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
        self.child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>)
    }
    async fn wait(&mut self) -> i32 {
        let code = self
            .child
            .wait()
            .await
            .ok()
            .and_then(|s| s.code())
            .unwrap_or(-1);
        if self.kill_requested {
            // The `start_kill` SIGKILL stops only the local client; the
            // daemon-side container outlives it. Remove it explicitly and
            // AWAIT the `rm` so a caller that proceeds straight to shutdown
            // can't race a detached cleanup into an orphaned container.
            let _ = Command::new(&self.binary)
                .args(["rm", "-f", &self.container_name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        }
        // Removed just above (on kill) or by `--rm` (clean exit) — no
        // drop-time `rm` needed.
        self.cleanup.disarm();
        code
    }
    fn start_kill(&mut self) {
        // SIGKILL the local `docker run` client and flag the container for
        // removal in `wait` (awaited, race-free). The drop guard stays ARMED
        // until `wait` disarms it, so a child dropped without `wait` still gets
        // a best-effort cleanup instead of leaking the container.
        self.kill_requested = true;
        let _ = self.child.start_kill();
    }
}

fn unique_container_name() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("baybo-sandbox-{}-{nanos:x}-{seq:x}", std::process::id())
}

#[cfg(test)]
mod detached_tests {
    use super::*;
    use crate::spec::{EnvPolicy, FilesystemPolicy, NetworkPolicy, ResourceLimits, StdinSource};
    use std::collections::BTreeSet;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    /// A runner pinned to a tiny image, or `None` when docker / the image is
    /// unavailable (the test then skips). Pre-pulls busybox so the run isn't
    /// at the mercy of a mid-test pull.
    async fn runner_or_skip() -> Option<DockerRunner> {
        let runner = DockerRunner::discover().ok()?.with_image("busybox:latest");
        let pulled = Command::new("docker")
            .args(["pull", "busybox:latest"])
            .output()
            .await
            .ok()?;
        pulled.status.success().then_some(runner)
    }

    fn spec(cmd: &str) -> SandboxSpec {
        SandboxSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), cmd.into()],
            cwd: None,
            workspace_root: std::env::temp_dir(),
            readable_paths: vec![],
            writable_paths: vec![],
            allowed_hosts: BTreeSet::new(),
            network_policy: NetworkPolicy::None,
            env: EnvPolicy::Baseline,
            stdin: StdinSource::Null,
            timeout: Duration::from_secs(30),
            resource_limits: ResourceLimits::unlimited(),
            filesystem_policy: FilesystemPolicy::default(),
        }
    }

    #[tokio::test]
    #[ignore = "requires a running docker daemon + network; run with --ignored"]
    async fn detached_docker_streams_output_and_exits() {
        let Some(runner) = runner_or_skip().await else {
            eprintln!("docker unavailable; skipping");
            return;
        };
        let mut child = runner
            .spawn_detached(spec("echo detached-ok"))
            .await
            .expect("spawn_detached");
        let mut stdout = child.take_stdout().expect("stdout pipe");
        let code = child.wait().await;
        let mut buf = String::new();
        stdout.read_to_string(&mut buf).await.expect("read stdout");
        assert_eq!(code, 0, "clean exit");
        assert!(buf.contains("detached-ok"), "stdout was: {buf:?}");
    }

    #[tokio::test]
    #[ignore = "requires a running docker daemon + network; run with --ignored"]
    async fn detached_docker_start_kill_stops_the_run() {
        let Some(runner) = runner_or_skip().await else {
            eprintln!("docker unavailable; skipping");
            return;
        };
        let mut child = runner
            .spawn_detached(spec("sleep 60"))
            .await
            .expect("spawn_detached");
        child.start_kill();
        // The killed `docker run` client must return promptly (not hang for
        // the full 60s sleep); `docker rm -f` tears the container down.
        let waited = tokio::time::timeout(Duration::from_secs(20), child.wait()).await;
        assert!(waited.is_ok(), "start_kill must let wait() return promptly");
    }
}
