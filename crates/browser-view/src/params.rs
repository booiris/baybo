//! Where the gateway listens for the browser sidecar's link, and the secret
//! the sidecar must present. Resolved once per gateway process: the same
//! value feeds the sidecar's env and the hub's listener, and must not change
//! for the process lifetime (the MCP reconciler hashes the env).

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rand::Rng;
use sha2::{Digest, Sha256};

use crate::error::BrowserViewError;
use crate::wire::LinkSecret;

/// Directory under the workspace state dir holding the link socket.
const LINK_SOCKET_DIR: &str = "browser-link";

const LINK_SOCKET_FILE: &str = "link.sock";

/// Subdirectory of `$XDG_RUNTIME_DIR` used when the state-dir path is too
/// long for `sun_path`.
const RUNTIME_DIR_SUBDIR: &str = "baybo";

const XDG_RUNTIME_DIR_ENV: &str = "XDG_RUNTIME_DIR";

/// Last-resort parent; the per-uid directory below it is created `0700`.
const TMP_ROOT: &str = "/tmp";

const TMP_DIR_PREFIX: &str = "baybo-";

const FALLBACK_SOCKET_EXT: &str = "sock";

/// Hex chars of the preferred path's SHA-256 used to name a fallback
/// socket, so two workspaces never share one.
const FALLBACK_HASH_HEX_CHARS: usize = 16;

/// Longest socket path both Linux (108-byte `sun_path`) and macOS (104)
/// accept, leaving room for the trailing NUL.
pub(crate) const MAX_SOCKET_PATH_BYTES: usize = 103;

const LINK_SECRET_BYTES: usize = 32;

/// The gateway's end of the browser link: socket path plus the one-time
/// secret. `Debug` is safe: [`LinkSecret`] redacts itself.
#[derive(Debug, Clone)]
pub struct BrowserLinkParams {
    socket: PathBuf,
    secret: LinkSecret,
}

impl BrowserLinkParams {
    /// Pick the socket path for a workspace whose state dir is `state_dir`
    /// and mint a fresh secret. Prefers `<state>/browser-link/link.sock`;
    /// when that overflows `sun_path` falls back to
    /// `$XDG_RUNTIME_DIR/baybo/<hash>.sock`, then `/tmp/baybo-<uid>/<hash>.sock`.
    pub fn resolve(state_dir: &Path) -> Result<Self, BrowserViewError> {
        let runtime_dir = std::env::var_os(XDG_RUNTIME_DIR_ENV).map(PathBuf::from);
        let socket = pick_socket_path(
            &state_dir.join(LINK_SOCKET_DIR).join(LINK_SOCKET_FILE),
            runtime_dir.as_deref(),
            current_uid(),
        )?;
        Ok(Self {
            socket,
            secret: generate_secret(),
        })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn secret(&self) -> &LinkSecret {
        &self.secret
    }
}

pub(crate) fn current_uid() -> u32 {
    // Safety: `getuid` has no preconditions and cannot fail per POSIX.
    unsafe { libc::getuid() }
}

fn fits(path: &Path) -> bool {
    path.as_os_str().len() <= MAX_SOCKET_PATH_BYTES
}

fn pick_socket_path(
    preferred: &Path,
    runtime_dir: Option<&Path>,
    uid: u32,
) -> Result<PathBuf, BrowserViewError> {
    if fits(preferred) {
        return Ok(preferred.to_path_buf());
    }
    let digest = hex::encode(Sha256::digest(preferred.as_os_str().as_bytes()));
    let file = format!(
        "{}.{FALLBACK_SOCKET_EXT}",
        &digest[..FALLBACK_HASH_HEX_CHARS]
    );
    let runtime = runtime_dir
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(RUNTIME_DIR_SUBDIR).join(&file));
    let tmp = Path::new(TMP_ROOT)
        .join(format!("{TMP_DIR_PREFIX}{uid}"))
        .join(&file);
    runtime
        .into_iter()
        .chain([tmp])
        .find(|candidate| fits(candidate))
        .ok_or(BrowserViewError::SocketPathTooLong {
            max: MAX_SOCKET_PATH_BYTES,
        })
}

fn generate_secret() -> LinkSecret {
    let mut bytes = [0u8; LINK_SECRET_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    LinkSecret::new(hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long_dir() -> PathBuf {
        PathBuf::from(format!("/{}", "d".repeat(MAX_SOCKET_PATH_BYTES)))
    }

    #[test]
    fn short_state_dir_uses_the_preferred_path() {
        let params = BrowserLinkParams::resolve(Path::new("/ws/state")).unwrap();
        assert_eq!(
            params.socket(),
            Path::new("/ws/state/browser-link/link.sock")
        );
    }

    #[test]
    fn long_path_falls_back_to_runtime_dir() {
        let preferred = long_dir().join(LINK_SOCKET_FILE);
        let path = pick_socket_path(&preferred, Some(Path::new("/run/user/1000")), 1000).unwrap();
        assert!(path.starts_with("/run/user/1000/baybo"), "{path:?}");
        assert!(fits(&path));
        let again = pick_socket_path(&preferred, Some(Path::new("/run/user/1000")), 1000).unwrap();
        assert_eq!(path, again, "fallback name is stable per workspace");
        let other = pick_socket_path(
            &long_dir().join("other.sock"),
            Some(Path::new("/run/user/1000")),
            1000,
        )
        .unwrap();
        assert_ne!(path, other, "fallback name differs per workspace");
    }

    #[test]
    fn long_path_without_runtime_dir_falls_back_to_tmp() {
        let preferred = long_dir().join(LINK_SOCKET_FILE);
        for runtime in [
            None,
            Some(Path::new("relative/run")),
            Some(long_dir().as_path()),
        ] {
            let path = pick_socket_path(&preferred, runtime, 4242).unwrap();
            assert!(path.starts_with("/tmp/baybo-4242"), "{path:?}");
            assert_eq!(
                path.extension().and_then(|e| e.to_str()),
                Some(FALLBACK_SOCKET_EXT)
            );
        }
    }

    #[test]
    fn secrets_are_fresh_and_non_empty() {
        let a = BrowserLinkParams::resolve(Path::new("/ws")).unwrap();
        let b = BrowserLinkParams::resolve(Path::new("/ws")).unwrap();
        assert_eq!(a.secret().expose().len(), 2 * LINK_SECRET_BYTES);
        assert!(!a.secret().ct_eq(b.secret()));
    }
}
