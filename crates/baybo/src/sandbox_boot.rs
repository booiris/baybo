//! Boot-time policy for the optional inner OS sandbox used by Bash.

use std::path::Path;
use std::sync::Arc;

use tracing::{info, warn};

const DOCKER_ENV_MARKER: &str = "/.dockerenv";
const CONTAINER_ENV_MARKER: &str = "/run/.containerenv";
const SINGULARITY_ENV_MARKER: &str = "/.singularity.d";

pub(crate) struct SandboxBoot {
    pub runner: Option<Arc<dyn baybo_sandbox::SandboxRunner>>,
    pub bypass_reason: Option<String>,
}

pub(crate) async fn resolve_sandbox_runner(
    permission: baybo_config::PermissionPolicy,
) -> Result<SandboxBoot, baybo_sandbox::SandboxError> {
    if cfg!(feature = "bench-bash") {
        // The bench profile ignores `permission` entirely and runs raw inside a
        // disposable container, where bwrap usually cannot nest.
        return Ok(without_runner(None));
    }

    if let Some(kind) = ambient_sandbox_kind() {
        warn!(
            ambient_sandbox = kind,
            "detected an outer sandbox/container; skipping the inner OS sandbox"
        );
        return Ok(without_runner(None));
    }

    let sandbox_required = permission != baybo_config::PermissionPolicy::Free;
    match baybo_sandbox::current_platform_runner() {
        Ok(runner) => match runner.warm().await {
            Ok(()) => {
                info!(backend = ?runner.backend(), "OS sandbox ready");
                Ok(SandboxBoot {
                    runner: Some(runner),
                    bypass_reason: None,
                })
            }
            Err(e) => {
                let reason = format!(
                    "OS sandbox backend {:?} failed warm-up: {e}. Baybo will run Bash without \
                     the inner OS sandbox.",
                    runner.backend()
                );
                if sandbox_required {
                    warn!(
                        error = %e,
                        backend = ?runner.backend(),
                        "sandbox warm-up failed; Bash will emit a notice and run without the inner OS sandbox",
                    );
                } else {
                    warn!(
                        error = %e,
                        backend = ?runner.backend(),
                        "sandbox warm-up failed; hot-reloading permission to auto/manual will run without the inner OS sandbox after a notice",
                    );
                }
                Ok(without_runner(Some(reason)))
            }
        },
        Err(
            e @ (baybo_sandbox::SandboxError::BackendMissing { .. }
            | baybo_sandbox::SandboxError::BackendUnreachable { .. }
            | baybo_sandbox::SandboxError::NoBackendAvailable),
        ) => {
            let reason = format!(
                "OS sandbox backend unavailable: {e}. Install bwrap (Linux: `apt install \
                 bubblewrap`), sandbox-exec (macOS, ships with the system), or docker, then \
                 restart Baybo to restore the inner OS sandbox."
            );
            if sandbox_required {
                warn!(
                    error = %e,
                    "OS sandbox unavailable; Bash will emit a notice and run without the inner OS sandbox",
                );
            } else {
                warn!(
                    error = %e,
                    "OS sandbox unavailable; hot-reloading permission to auto/manual will run without the inner OS sandbox after a notice",
                );
            }
            Ok(without_runner(Some(reason)))
        }
        Err(e) => Err(e),
    }
}

fn without_runner(bypass_reason: Option<String>) -> SandboxBoot {
    SandboxBoot {
        runner: None,
        bypass_reason,
    }
}

fn ambient_sandbox_kind() -> Option<&'static str> {
    let cgroup = std::fs::read_to_string("/proc/1/cgroup")
        .or_else(|_| std::fs::read_to_string("/proc/self/cgroup"))
        .unwrap_or_default();
    ambient_sandbox_kind_from_signals(
        Path::new(DOCKER_ENV_MARKER).exists(),
        Path::new(CONTAINER_ENV_MARKER).exists(),
        Path::new(SINGULARITY_ENV_MARKER).exists(),
        &cgroup,
    )
}

fn ambient_sandbox_kind_from_signals(
    has_dockerenv: bool,
    has_containerenv: bool,
    has_singularity: bool,
    cgroup: &str,
) -> Option<&'static str> {
    if has_dockerenv {
        return Some("Docker");
    }
    if has_containerenv {
        return Some("container runtime");
    }
    if has_singularity {
        return Some("Singularity/Apptainer");
    }

    let cgroup = cgroup.to_ascii_lowercase();
    for (needle, kind) in [
        ("kubepods", "Kubernetes"),
        ("containerd", "containerd"),
        ("docker", "Docker"),
        ("libpod", "Podman"),
        ("podman", "Podman"),
        ("lxc", "LXC"),
    ] {
        if cgroup.contains(needle) {
            return Some(kind);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ambient_sandbox_detects_docker_marker() {
        assert_eq!(
            ambient_sandbox_kind_from_signals(true, false, false, ""),
            Some("Docker")
        );
    }

    #[test]
    fn ambient_sandbox_detects_kubernetes_cgroup() {
        assert_eq!(
            ambient_sandbox_kind_from_signals(
                false,
                false,
                false,
                "0::/kubepods.slice/kubepods-burstable.slice/pod123"
            ),
            Some("Kubernetes")
        );
    }

    #[test]
    fn ambient_sandbox_ignores_plain_host_cgroup() {
        assert_eq!(
            ambient_sandbox_kind_from_signals(false, false, false, "0::/"),
            None
        );
    }
}
