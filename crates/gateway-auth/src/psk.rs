//! Build-time PSK + per-install salt derivation for TUI authentication.
//!
//! # Threat model
//!
//! The effective TUI PSK is a **workspace-binding token**, not a defense
//! against a same-UID hostile process. It usefully rejects:
//!
//! * connections from a different Unix user (already gated by UDS
//!   `0o600` perms + `SO_PEERCRED`; the PSK is the belt on top of those
//!   suspenders);
//! * connections that crossed a workspace boundary (a TUI built against
//!   workspace A will not authenticate against the gateway running in
//!   workspace B, because the per-install salt file diverges);
//! * stale reconnects from an older build or a different on-disk install
//!   (different embedded PSK bytes).
//!
//! It explicitly does **not** defend against a malicious process running
//! as the same UID as the gateway. Such a process can read the salt file
//! and the installed binary, recompute the effective PSK, and present
//! it. Treat "same-UID adversary" as outside the scope of this
//! mechanism — file permissions, service-manager sandboxing, and OS-
//! level isolation are the tools for that threat.

use std::fs;
use std::io;
use std::path::Path;

use hkdf::Hkdf;
use rand::Rng;
use sha2::Sha256;

/// HTTP header the TUI uses to present the derived PSK.
pub const TUI_PSK_HEADER: &str = "x-aura-tui-secret";

/// Compiled-in 32-byte PSK, regenerated on every build.
///
/// Direct use is discouraged — call [`effective_tui_psk`] instead so the
/// per-install salt is mixed in.
pub const EMBEDDED_TUI_PSK: &[u8; 32] = include_bytes!(concat!(env!("OUT_DIR"), "/tui_psk.bin"));

const SALT_FILE_NAME: &str = "tui_psk.salt";
const HKDF_INFO: &[u8] = b"aura-gateway tui psk v1";

/// Read or create the workspace-local salt file, then derive the
/// effective TUI PSK via HKDF-SHA256(ikm=EMBEDDED_TUI_PSK, salt=salt).
///
/// * `workspace_dir` is the per-workspace identity directory (the same
///   one that holds the gateway singleton lock and the channel UDS).
/// * If the salt file is missing, 32 random bytes are written with mode
///   0600. Concurrent writers race benignly: the first write wins and
///   subsequent readers see the same value.
/// * A read failure (permission, short read, …) is surfaced instead of
///   silently falling back to an un-salted PSK — mismatched salt files
///   between TUI and gateway would cause auth failures, and a loud error
///   is easier to diagnose than a silent 401.
pub fn effective_tui_psk(workspace_dir: &Path) -> io::Result<[u8; 32]> {
    let salt_path = workspace_dir.join(SALT_FILE_NAME);
    let salt = read_or_create_salt(&salt_path)?;

    let hk = Hkdf::<Sha256>::new(Some(&salt), EMBEDDED_TUI_PSK);
    let mut out = [0u8; 32];
    hk.expand(HKDF_INFO, &mut out)
        .expect("32-byte HKDF output fits within Sha256::OutputSize * 255");
    Ok(out)
}

fn read_or_create_salt(path: &Path) -> io::Result<[u8; 32]> {
    match fs::read(path) {
        Ok(bytes) if bytes.len() == 32 => {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            Ok(arr)
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: unexpected salt length; delete the file to regenerate",
                path.display()
            ),
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => create_salt(path),
        Err(e) => Err(e),
    }
}

fn create_salt(path: &Path) -> io::Result<[u8; 32]> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    write_mode_0600(path, &bytes)?;
    Ok(bytes)
}

fn write_mode_0600(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)?;
    f.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn embedded_psk_is_32_bytes() {
        assert_eq!(EMBEDDED_TUI_PSK.len(), 32);
    }

    #[test]
    fn effective_psk_is_deterministic_per_workspace() {
        let dir = tempdir().unwrap();
        let a = effective_tui_psk(dir.path()).unwrap();
        let b = effective_tui_psk(dir.path()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn different_workspaces_derive_different_psks() {
        let d1 = tempdir().unwrap();
        let d2 = tempdir().unwrap();
        let a = effective_tui_psk(d1.path()).unwrap();
        let b = effective_tui_psk(d2.path()).unwrap();
        assert_ne!(a, b, "per-install salt must diverge PSKs");
    }

    #[test]
    fn salt_file_survives_restart() {
        let dir = tempdir().unwrap();
        let a = effective_tui_psk(dir.path()).unwrap();
        // Second invocation reads the existing salt — identical output
        // is the contract the TUI/gateway rely on.
        let b = effective_tui_psk(dir.path()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn salt_file_is_mode_0600() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempdir().unwrap();
        let _ = effective_tui_psk(dir.path()).unwrap();
        let meta = fs::metadata(dir.path().join(SALT_FILE_NAME)).unwrap();
        // Low 9 bits = permission bits; 0o600 = owner rw only.
        assert_eq!(meta.mode() & 0o777, 0o600);
    }
}
