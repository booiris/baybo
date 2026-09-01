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
    /// `PATH` the service manager must hand the daemon, rendered into
    /// the unit. Built by [`resolve_service_path`] at install time —
    /// service managers supply their own minimal `PATH` (a systemd user
    /// unit gets `/usr/local/bin:/usr/bin`), never the operator's, so
    /// without this line the daemon cannot find any host tool installed
    /// under `$HOME`.
    pub path_env: String,
    /// The account a **system-wide** unit must drop to, resolved by
    /// [`resolve_service_user`]. `None` for a per-user install, where
    /// the service manager already runs as the right user and the unit
    /// must not name one.
    pub run_as: Option<ServiceUser>,
}

/// The account a system-wide unit runs the gateway as.
///
/// A `[Service]` block with no `User=` runs as **root** — while the
/// paths baked into that same unit came from whoever ran the install.
/// The default is therefore a root daemon writing a user-owned
/// workspace, leaving root-owned `storage.db`, vault and `personas/`
/// that the user's own CLI and TUI can no longer write. Construction
/// goes through [`resolve_service_user`] so that outcome can only ever
/// be reached by asking for it.
#[derive(Debug, Clone)]
pub struct ServiceUser(String);

impl ServiceUser {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Resolve the account a `--system` install pins the service to.
///
/// `getuid()` is deliberately **not** consulted. `--system` writes
/// `/etc/systemd/system`, so it can only succeed when already root, and
/// `User=0` is root — the exact bug this exists to prevent. The real
/// operator is whoever escalated: `SUDO_USER` under `sudo`,
/// `PKEXEC_UID` under `pkexec` (numeric, which systemd accepts).
///
/// When neither is present — a plain root login, a container, a
/// provisioning script — there is no honest answer, so this **fails**
/// and points at `--run-as`. Silently emitting a root unit is what got
/// us here; making the operator name the account takes one flag and
/// makes a root daemon an explicit choice (`--run-as root`) instead of
/// the default nobody picked.
pub fn resolve_service_user(explicit: Option<&str>) -> Result<ServiceUser> {
    fn accept(raw: &str, source: &str) -> Result<ServiceUser> {
        let name = raw.trim();
        // The value lands verbatim on a `User=` line; anything with
        // whitespace or a separator in it would silently produce a unit
        // that means something other than what it says.
        if name.is_empty() || name.chars().any(|c| c.is_whitespace() || c == '=') {
            return Err(InstallerError::Other(format!(
                "{source} is not a usable account name ({raw:?}); pass --run-as <user>"
            )));
        }
        Ok(ServiceUser(name.to_string()))
    }

    if let Some(name) = explicit {
        return accept(name, "--run-as");
    }
    if let Some(name) = std::env::var_os("SUDO_USER").and_then(|v| v.into_string().ok()) {
        return accept(&name, "SUDO_USER");
    }
    if let Some(uid) = std::env::var_os("PKEXEC_UID").and_then(|v| v.into_string().ok()) {
        return accept(&uid, "PKEXEC_UID");
    }
    Err(InstallerError::Other(
        "cannot tell which account the system service should run as: neither SUDO_USER nor \
         PKEXEC_UID is set. A system unit with no `User=` runs as root and would write \
         root-owned files into the workspace this unit points at. Re-run with \
         `--run-as <user>` (use `--run-as root` if a root-owned workspace is what you want)."
            .into(),
    ))
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
    baybo_process::lookup_on_path("baybo")
}

/// Host binaries the gateway shells out to at runtime. Kept in sync with
/// the inventory in `docs/external-commands.md`; the daemon's `PATH` is
/// derived from where *these* actually live, never from a snapshot of
/// the operator's own `PATH`.
///
/// Two entries are load-bearing beyond baybo's own spawns: `claude` and
/// `codex` are probed by name from deck cards and skills via `sh -c`,
/// so dropping them here would revive the "CLI is not installed" class
/// of failure one layer above the ones baybo spawns itself.
const SERVICE_PATH_TOOLS: &[&str] = &[
    "bun",
    "node",
    "uv",
    "git",
    "rg",
    "sh",
    "tmux",
    "bwrap",
    "sandbox-exec",
    "docker",
    "systemd-run",
    "claude",
    "codex",
];

/// System bin dirs appended after the discovered ones, so a tool
/// installed *after* the install still resolves.
const SERVICE_PATH_SYSTEM_DIRS: &[&str] = &[
    "/usr/local/bin",
    "/usr/local/sbin",
    "/usr/bin",
    "/usr/sbin",
    "/bin",
    "/sbin",
];

/// Build the `PATH` to bake into the generated unit.
///
/// Derived from *need*, not from inheritance: walk the installing
/// process's `PATH` in order and keep only the dirs that actually hold
/// one of [`SERVICE_PATH_TOOLS`], then append the system bin dirs. A
/// real operator `PATH` is 20+ entries of toolchain shims, SDK dirs and
/// editor-server paths carrying an embedded commit hash — snapshotting
/// it whole would bake in entries that are stale by the next upgrade,
/// while keeping the order means the service resolves the same `bun` the
/// operator's shell does.
pub fn resolve_service_path() -> String {
    let path = std::env::var_os("PATH").unwrap_or_default();
    service_path_from(std::env::split_paths(&path))
}

/// The selection itself, over an explicit candidate list so it is
/// testable without mutating the process's `PATH`.
fn service_path_from(candidates: impl Iterator<Item = PathBuf>) -> String {
    fn push(dirs: &mut Vec<PathBuf>, dir: PathBuf) {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }

    let mut dirs: Vec<PathBuf> = Vec::new();
    for dir in candidates {
        // A relative entry would resolve against the daemon's cwd, which
        // is not the operator's — never propagate one into a unit.
        if !dir.is_absolute() {
            continue;
        }
        if SERVICE_PATH_TOOLS
            .iter()
            .any(|tool| baybo_process::is_executable(&dir.join(tool)))
        {
            push(&mut dirs, dir);
        }
    }
    for dir in SERVICE_PATH_SYSTEM_DIRS {
        let dir = PathBuf::from(dir);
        if dir.is_dir() {
            push(&mut dirs, dir);
        }
    }

    // `join_paths` only fails on a `:` inside an entry, which would also
    // have corrupted the unit — fall back to the system dirs alone.
    std::env::join_paths(&dirs)
        .map(|joined| joined.to_string_lossy().into_owned())
        .unwrap_or_else(|_| SERVICE_PATH_SYSTEM_DIRS.join(":"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generated `PATH` must be usable as-is: non-empty, and every
    /// entry absolute.
    #[test]
    fn resolved_service_path_is_absolute_and_non_empty() {
        let path = resolve_service_path();
        assert!(!path.is_empty(), "service PATH must never render empty");
        for dir in path.split(':') {
            assert!(
                dir.starts_with('/'),
                "service PATH entry {dir:?} is not absolute (from {path:?})"
            );
        }
    }

    /// Discovery is keyed on the tools, not on inheritance: a dir holding
    /// none of them is dropped rather than copied into the unit, so
    /// editor-server and SDK dirs (whose names embed a version or commit
    /// hash) never get baked in to rot.
    #[test]
    fn service_path_drops_dirs_without_tools() {
        let dir = tempfile::tempdir().expect("tempdir");
        let junk = dir.path().join("vscode-server-abc123");
        std::fs::create_dir_all(&junk).expect("mkdir");
        let path = service_path_from(std::iter::once(junk));
        assert!(
            !path.contains("vscode-server-abc123"),
            "a dir holding no service tool must not reach the unit, got {path:?}"
        );
    }

    /// An explicit `--run-as` always wins, including the deliberate
    /// `--run-as root` for a genuinely root-owned deployment.
    #[test]
    fn explicit_run_as_wins() {
        assert_eq!(
            resolve_service_user(Some("booiris"))
                .expect("explicit")
                .as_str(),
            "booiris"
        );
        assert_eq!(
            resolve_service_user(Some("root"))
                .expect("explicit root")
                .as_str(),
            "root",
            "a root service must remain reachable, just never by default"
        );
    }

    /// A value that would change the meaning of the `User=` line is
    /// rejected rather than rendered.
    #[test]
    fn run_as_rejects_values_that_would_corrupt_the_unit() {
        for bad in ["", "  ", "me and you", "me\nExecStart=/bin/sh", "a=b"] {
            assert!(
                resolve_service_user(Some(bad)).is_err(),
                "{bad:?} must not reach a User= line"
            );
        }
    }

    /// …and a dir that *does* hold one is kept, ahead of the system
    /// dirs, so the service resolves the same binary the operator's
    /// shell does.
    #[test]
    fn service_path_keeps_tool_dir_ahead_of_system_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir");
        let bun = bin.join("bun");
        std::fs::write(&bun, "#!/bin/sh\n").expect("write");
        let mut perms = std::fs::metadata(&bun).expect("meta").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&bun, perms).expect("chmod");

        let path = service_path_from(std::iter::once(bin.clone()));
        let entries: Vec<&str> = path.split(':').collect();
        assert_eq!(
            entries.first().map(PathBuf::from),
            Some(bin),
            "a discovered tool dir must lead the service PATH, got {path:?}"
        );
        assert!(
            entries.contains(&"/usr/bin"),
            "system dirs must still be appended, got {path:?}"
        );
    }
}
