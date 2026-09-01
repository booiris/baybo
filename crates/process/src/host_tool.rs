//! Locating the host tools baybo shells out to.
//!
//! A service manager hands the daemon a `PATH` of its own choosing: a
//! systemd user unit inherits the user manager's default
//! (`/usr/local/bin:/usr/bin`), a launchd agent gets less still. A `bun`
//! dropped under `~/.local/bin` by its own installer is therefore
//! invisible to `Command::new("bun")` even though the operator's shell
//! finds it instantly — the failure reads as "bun is not installed"
//! when it plainly is.
//!
//! So resolution ends with an explicit look under the well-known
//! per-user install roots before giving up, and every caller reports the
//! miss with the same sentence. The install-time fix (a `PATH=` line in
//! the generated unit, see `baybo-gateway`'s installer) is the real one;
//! this is what keeps an already-installed unit working.

use std::path::{Path, PathBuf};

/// Override for the `bun` binary. One name across every call site that
/// needs bun — channel sidecars, deck card services, the channel
/// registration flow — because "the bun that runs embedded JS" is one
/// operator concept, not three.
pub const BUN_BINARY_ENV: &str = "BAYBO_BUN_BIN";

/// Override for the `node` binary used by embedded MCP sidecars.
pub const NODE_BINARY_ENV: &str = "BAYBO_NODE_BIN";

/// `$HOME`-relative dirs a JS runtime lands in when installed by its own
/// installer rather than a distro package. Consulted only after `PATH`
/// misses, so a packaged install always wins.
const HOME_INSTALL_DIRS: &[&str] = &[".local/bin", ".bun/bin"];

/// A host binary the runtime spawns, resolved to a concrete path.
///
/// Resolve once per spawn attempt (not once per process): an operator
/// who installs the missing tool should not have to restart the daemon.
#[derive(Debug, Clone)]
pub struct HostTool {
    name: &'static str,
    override_env: &'static str,
    path: PathBuf,
}

impl HostTool {
    pub fn bun() -> Self {
        Self::resolve("bun", BUN_BINARY_ENV)
    }

    pub fn node() -> Self {
        Self::resolve("node", NODE_BINARY_ENV)
    }

    fn resolve(name: &'static str, override_env: &'static str) -> Self {
        let path = std::env::var_os(override_env)
            .map(PathBuf::from)
            .or_else(|| lookup_on_path(name))
            .or_else(|| lookup_in_home_installs(name))
            // Nothing hit: hand the bare name over anyway so the spawn
            // produces a real `ErrorKind::NotFound` for the caller to
            // classify, rather than us inventing one here.
            .unwrap_or_else(|| PathBuf::from(name));
        Self {
            name,
            override_env,
            path,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The one sentence every caller prints when the spawn fails: what
    /// was tried, and the knob that fixes it without a rebuild.
    pub fn launch_failure(&self, err: impl std::fmt::Display) -> String {
        format!(
            "failed to launch `{}` ({err}); is {} installed and on PATH? (override with {})",
            self.path.display(),
            self.name,
            self.override_env
        )
    }
}

/// Regular file with at least one execute bit set.
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// First executable named `name` on the current process's `PATH`.
pub fn lookup_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn lookup_in_home_installs(name: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    HOME_INSTALL_DIRS
        .iter()
        .map(|dir| Path::new(&home).join(dir).join(name))
        .find(|candidate| is_executable(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn touch_exec(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\n").expect("write");
        let mut perms = std::fs::metadata(&path).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
        path
    }

    #[test]
    fn is_executable_rejects_dirs_and_plain_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!is_executable(dir.path()));

        let plain = dir.path().join("plain");
        std::fs::write(&plain, "x").expect("write");
        assert!(!is_executable(&plain));

        assert!(is_executable(&touch_exec(dir.path(), "runme")));
    }

    #[test]
    fn is_executable_rejects_missing() {
        assert!(!is_executable(Path::new("/definitely/not/here/baybo-xyz")));
    }
}
