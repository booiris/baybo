//! Platform service installers for the gateway.
//!
//! Per-OS implementations live in sibling modules gated by Cargo
//! features (`linux`, `macos`). The factory [`for_current_platform`]
//! picks the right impl at runtime; callers that target a platform with
//! no compiled-in installer receive [`InstallerError::Unsupported`].
//!
//! Any future OS-specific code the gateway grows should reuse these
//! same feature gates rather than introducing narrower flags — one knob
//! per OS keeps cross-platform builds tractable.

use std::path::{Path, PathBuf};

use thiserror::Error;

#[cfg(all(target_os = "linux", feature = "linux"))]
pub mod systemd;

#[cfg(all(target_os = "macos", feature = "macos"))]
pub mod launchd;

pub type Result<T> = std::result::Result<T, InstallerError>;

#[derive(Debug, Error)]
pub enum InstallerError {
    #[error("service install is not supported on {0}")]
    Unsupported(&'static str),

    #[error("cannot resolve executable path: {0}. Pass --exec-start or install a release build.")]
    ExecResolution(String),

    #[error("io error on {path}: {reason}")]
    Io { path: String, reason: String },

    #[error("external command `{cmd}` failed (exit {status}): {stderr}")]
    External {
        cmd: String,
        status: String,
        stderr: String,
    },

    #[error("systemd install requires a HOME directory; $HOME is not set")]
    NoHome,

    #[error("{0}")]
    Other(String),
}

/// Inputs the installer needs to render a unit file. Captured at
/// `install` time so the resulting service file is reproducible.
#[derive(Debug, Clone)]
pub struct InstallContext {
    /// Resolved absolute path to the `baybo` binary that the service
    /// invokes. Set by [`resolve_exec_start`].
    pub exec_start: PathBuf,
    /// Absolute path to the baybo config file the service should load.
    /// Rendered as `Environment=BAYBO_CONFIG_PATH=…` (systemd) or an
    /// `EnvironmentVariables` dict entry (launchd) in the unit file —
    /// systemd/launchd do not inherit the invoking shell's env, so the
    /// caller must capture `BAYBO_CONFIG_PATH` at install time.
    pub config_path: Option<PathBuf>,
    /// Log directory; surfaced as a hint in the rendered unit file.
    pub log_dir: PathBuf,
    /// User-mode install (`--user` on Linux). `false` writes to the
    /// system unit directory (requires root).
    pub user_mode: bool,
}

/// Runtime status of the installed service.
#[derive(Debug, Clone)]
pub enum ServiceStatus {
    /// Unit file not present.
    NotInstalled,
    /// Unit installed but not enabled for autostart.
    Installed,
    /// Enabled; will start at boot.
    Enabled,
    /// Enabled and currently running.
    Running,
    /// Something went wrong inspecting status.
    Unknown(String),
}

pub trait ServiceInstaller {
    fn unit_path(&self) -> PathBuf;
    fn render_unit(&self, ctx: &InstallContext) -> String;
    fn install(&self, ctx: &InstallContext) -> Result<PathBuf>;
    fn enable(&self) -> Result<()>;
    fn restart(&self) -> Result<()>;
    fn disable(&self) -> Result<()>;
    fn uninstall(&self) -> Result<()>;
    fn status(&self) -> Result<ServiceStatus>;
}

/// Return the installer for the current target OS, or
/// [`InstallerError::Unsupported`] if no installer is compiled-in.
///
/// The `return`s below look unconditional but only one `cfg` branch is
/// compiled on any given target; clippy can't see that.
#[allow(clippy::needless_return)]
pub fn for_current_platform(user_mode: bool) -> Result<Box<dyn ServiceInstaller>> {
    #[cfg(all(target_os = "linux", feature = "linux"))]
    {
        let _ = user_mode;
        return Ok(Box::new(systemd::SystemdInstaller::new(user_mode)));
    }
    #[cfg(all(target_os = "macos", feature = "macos"))]
    {
        let _ = user_mode;
        return Ok(Box::new(launchd::LaunchdInstaller::new()));
    }
    #[cfg(not(any(
        all(target_os = "linux", feature = "linux"),
        all(target_os = "macos", feature = "macos"),
    )))]
    {
        let _ = user_mode;
        Err(InstallerError::Unsupported(std::env::consts::OS))
    }
}

/// Resolve the `ExecStart` path for the generated unit. Precedence:
///
/// 1. Explicit `--exec-start <path>` (if provided).
/// 2. `which baybo` via PATH lookup.
/// 3. `std::env::current_exe()`.
///
/// Under debug builds (`cfg(debug_assertions)`) with no explicit
/// override, we refuse — `target/debug/baybo` vanishes after
/// `cargo clean` and would leave the user with a broken service.
pub fn resolve_exec_start(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        let abs = path
            .canonicalize()
            .map_err(|e| InstallerError::ExecResolution(format!("{}: {e}", path.display())))?;
        return Ok(abs);
    }

    if cfg!(debug_assertions) {
        return Err(InstallerError::ExecResolution(
            "running in dev build; pass --exec-start <path> or install a release build".into(),
        ));
    }

    if let Some(path) = which_baybo() {
        return Ok(path);
    }

    let current = std::env::current_exe()
        .map_err(|e| InstallerError::ExecResolution(format!("current_exe: {e}")))?;
    Ok(current)
}

fn which_baybo() -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join("baybo");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
