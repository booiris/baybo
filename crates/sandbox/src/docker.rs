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
use crate::spec::{Backend, SandboxOutput, SandboxSpec, StdinSource};

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
const DEFAULT_IMAGE: &str = "debian:stable-slim";

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

    /// Best-effort cleanup of a daemon-side container. Called when
    /// `run()` is unwinding through a timeout or io error so the
    /// container cannot keep writing into the workspace bind after
    /// we have already returned to the agent.
    fn force_remove_container(&self, name: &str) {
        let _ = std::process::Command::new(&self.binary)
            .args(["rm", "-f", name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[async_trait]
impl SandboxRunner for DockerRunner {
    async fn warm(&self) -> Result<(), SandboxError> {
        self.pin_image().await
    }

    async fn run(&self, spec: SandboxSpec) -> Result<SandboxOutput, SandboxError> {
        let container_name = unique_container_name();
        let argv = build_docker_argv(&spec, self.effective_image(), &container_name);
        let mut cmd = Command::new(&self.binary);
        cmd.args(&argv)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd.stdin(match spec.stdin {
            StdinSource::Null => Stdio::null(),
            StdinSource::Inherit => Stdio::inherit(),
            StdinSource::Bytes(_) => Stdio::piped(),
        });

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
                // doesn't mis-attribute it to the user command.
                if !out.status.success() && exit_code == 125 && !out.stderr.is_empty() {
                    self.force_remove_container(&container_name);
                    return Err(SandboxError::BackendFailure {
                        status: out.status.code(),
                        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
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
            Ok(Err(e)) => {
                self.force_remove_container(&container_name);
                Err(SandboxError::Io(e))
            }
            Err(_) => {
                // The local docker CLI client is killed via
                // kill_on_drop, but the daemon-side container is
                // independent of our process tree. Without an
                // explicit `docker rm -f` it would keep running
                // — and writing into the workspace bind — long
                // after the agent has been told the tool timed
                // out. Force-remove before returning.
                self.force_remove_container(&container_name);
                Err(SandboxError::Timeout(spec.timeout))
            }
        }
    }

    fn backend(&self) -> Backend {
        Backend::Docker
    }
}

fn unique_container_name() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("aura-sandbox-{}-{nanos:x}-{seq:x}", std::process::id())
}
