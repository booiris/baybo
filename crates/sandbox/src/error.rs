use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("sandbox not supported on platform `{0}`")]
    UnsupportedPlatform(&'static str),

    #[error(
        "backend binary `{name}` not found in $PATH; install it (Linux: `apt install bubblewrap` / `dnf install bubblewrap`; macOS: ships with the system; or install docker as a cross-platform fallback) and restart aura"
    )]
    BackendMissing { name: &'static str },

    #[error(
        "no sandbox backend available on this host; install one of: bwrap (Linux: `apt install bubblewrap`), sandbox-exec (macOS, ships with the system), or docker, then restart aura"
    )]
    NoBackendAvailable,

    #[error("backend `{name}` is installed but unusable: {message}")]
    BackendUnreachable { name: &'static str, message: String },

    #[error("backend binary `{name}` at {path:?} is not executable")]
    BackendNotExecutable { name: &'static str, path: PathBuf },

    #[error("invalid sandbox spec: {0}")]
    InvalidSpec(String),

    /// The selected backend cannot enforce some part of the requested
    /// `SandboxSpec` (e.g. per-host network allowlist on bwrap, cgroup
    /// caps on a daemonised host without `systemd-run --user`). The
    /// runner refuses rather than silently downgrading, so the caller
    /// has a chance to either narrow the spec, switch backends, or
    /// explicitly opt into the unenforced state via
    /// `ResourceLimits::unlimited()` / empty `allowed_hosts`.
    #[error(
        "{backend} sandbox cannot enforce {what}; {hint} (or relax the spec to opt into the unenforced state)"
    )]
    Unenforceable {
        backend: &'static str,
        what: String,
        hint: &'static str,
    },

    #[error("io error launching sandbox: {0}")]
    Io(#[from] std::io::Error),

    #[error("sandboxed process timed out after {0:?}")]
    Timeout(Duration),

    #[error("sandbox backend setup failed (status {status:?}): {stderr}")]
    BackendFailure { status: Option<i32>, stderr: String },
}
