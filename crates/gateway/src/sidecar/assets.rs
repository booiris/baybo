//! Embedded-asset extraction: turns the zstd-compressed byte slices
//! baked in by `build.rs` into on-disk files the OS can actually
//! `execve` (for bun) or pass to an interpreter (for each sidecar's
//! JS bundle).
//!
//! Safety note: the bun binary is written to disk with mode `0700`
//! so only the invoking user can exec it. The sidecar JS files are
//! left at the default umask — they're plain data, not executable.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use aura_workspace::paths::{CACHE_RUNTIME_SUBDIR, aura_cache_root};
use thiserror::Error;

mod generated {
    // Populated by `crates/gateway/build.rs` — see that file for
    // the emitted shape. When the sidecar pipeline degrades (no
    // `.bun-version`, bun download failed, etc.) the generated file
    // still compiles, just with `BUN_RUNTIME_ZST = &[]` and
    // `SIDECARS = &[]`.
    include!(concat!(env!("OUT_DIR"), "/sidecar_assets.rs"));
}

pub(crate) use generated::BUN_VERSION;
use generated::{BUN_RUNTIME_ZST, BUN_TARGET, SIDECARS, SidecarAsset, SidecarAuxAsset};

#[derive(Debug, Error)]
pub enum SidecarError {
    #[error("no embedded bun runtime in this build (build.rs degraded); sidecars disabled")]
    RuntimeMissing,
    #[error("cannot locate cache directory: set $XDG_CACHE_HOME or $HOME")]
    NoCacheDir,
    #[error("io error under {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("zstd decompress failed for {what}: {source}")]
    Decompress {
        what: &'static str,
        #[source]
        source: io::Error,
    },
}

/// Materialised runtime: bun on disk + one JS path per embedded sidecar.
/// Cheap to hold (two paths + a small Vec); clone via `Arc` at the
/// call site if shared across tasks.
pub struct SidecarRuntime {
    bun_path: PathBuf,
    sidecars: Vec<(String, PathBuf)>,
}

impl SidecarRuntime {
    /// Materialise bun + every embedded sidecar bundle under the user
    /// cache dir. Idempotent: repeated calls notice the existing files
    /// and skip the zstd decode. If the build has no embedded bun this
    /// returns [`SidecarError::RuntimeMissing`] — the caller is
    /// expected to log-and-skip rather than crash the gateway.
    pub fn install() -> Result<Self, SidecarError> {
        if BUN_RUNTIME_ZST.is_empty() {
            return Err(SidecarError::RuntimeMissing);
        }
        let cache_root = cache_root()?;
        let bun_path = install_bun(&cache_root)?;
        let mut sidecars = Vec::with_capacity(SIDECARS.len());
        for asset in SIDECARS {
            let js_path = install_sidecar(&cache_root, asset)?;
            sidecars.push((asset.channel_type.to_string(), js_path));
        }
        Ok(Self { bun_path, sidecars })
    }

    pub fn bun_path(&self) -> &Path {
        &self.bun_path
    }

    pub fn bun_target(&self) -> &'static str {
        BUN_TARGET
    }

    pub fn channel_types(&self) -> impl Iterator<Item = &str> {
        self.sidecars.iter().map(|(c, _)| c.as_str())
    }

    /// Path to the JS bundle for `channel_type`, or `None` if this
    /// build doesn't ship a sidecar for it.
    pub fn bundle_for(&self, channel_type: &str) -> Option<&Path> {
        self.sidecars
            .iter()
            .find(|(c, _)| c == channel_type)
            .map(|(_, p)| p.as_path())
    }
}

fn cache_root() -> Result<PathBuf, SidecarError> {
    aura_cache_root().ok_or(SidecarError::NoCacheDir)
}

fn install_bun(cache_root: &Path) -> Result<PathBuf, SidecarError> {
    let dir = cache_root.join(CACHE_RUNTIME_SUBDIR);
    mkdir_all(&dir)?;
    // Version AND target are in the filename so a sibling install on
    // a different target (bind-mounted home dir, etc.) doesn't trip
    // over ours.
    let dest = dir.join(format!("bun-{BUN_VERSION}-{BUN_TARGET}"));
    if dest.exists() {
        // Defensive repair: a previous install that crashed between
        // persist and chmod (older code path), or any other reason
        // the cached binary lost +x, would otherwise wedge the
        // supervisor in a permanent spawn-failure loop. Force the
        // mode back to 0o700 every boot — it's idempotent and cheap.
        ensure_mode(&dest, 0o700)?;
        return Ok(dest);
    }
    decompress_to(BUN_RUNTIME_ZST, &dest, "bun runtime", Some(0o700))?;
    Ok(dest)
}

fn install_sidecar(cache_root: &Path, asset: &SidecarAsset) -> Result<PathBuf, SidecarError> {
    let dir = cache_root
        .join("sidecars")
        .join(format!("{}-{}", asset.channel_type, asset.content_hash));
    mkdir_all(&dir)?;
    // Hash-keyed: a bundle content change lands at a fresh filename
    // without ever rewriting a live one. Auxiliary files live next to
    // index.js so packages that resolve assets from import.meta.url
    // can find them.
    let dest = dir.join("index.js");
    if !dest.exists() {
        decompress_to(asset.bundle_zst, &dest, "sidecar bundle", None)?;
    }
    for aux in asset.aux_assets {
        install_aux_asset(&dir, aux)?;
    }
    Ok(dest)
}

fn install_aux_asset(dir: &Path, asset: &SidecarAuxAsset) -> Result<(), SidecarError> {
    let rel = safe_relative_path(asset.name)?;
    let dest = dir.join(rel);
    if let Some(parent) = dest.parent() {
        mkdir_all(parent)?;
    }
    if dest.exists() {
        return Ok(());
    }
    decompress_to(asset.content_zst, &dest, "sidecar aux asset", None)
}

fn safe_relative_path(raw: &str) -> Result<&Path, SidecarError> {
    let path = Path::new(raw);
    if raw.is_empty() || path.is_absolute() {
        return Err(SidecarError::Io {
            path: PathBuf::from(raw),
            source: io::Error::new(io::ErrorKind::InvalidInput, "unsafe aux asset path"),
        });
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            _ => {
                return Err(SidecarError::Io {
                    path: PathBuf::from(raw),
                    source: io::Error::new(io::ErrorKind::InvalidInput, "unsafe aux asset path"),
                });
            }
        }
    }
    Ok(path)
}

fn decompress_to(
    zst: &[u8],
    dest: &Path,
    what: &'static str,
    mode: Option<u32>,
) -> Result<(), SidecarError> {
    // Per-process tempfile under the same dir as `dest`, atomically
    // persisted. Two concerns this addresses, both from
    // `$XDG_CACHE_HOME/aura/` being user-level shared across every
    // workspace the same UID runs:
    //
    //   1. Two gateways first-installing concurrently would both
    //      write a fixed `<dest>.part` and race on the rename. With
    //      NamedTempFile they each write a unique `.tmp<rand>` and
    //      the second `persist` atomically replaces the first's
    //      target (contents are identical — bytes come from the same
    //      embedded compressed blob — so either "winner" is fine).
    //   2. A crash mid-write leaves the NamedTempFile's `Drop`
    //      unlinking the partial, instead of a stale `<dest>.part`
    //      that a later run might mistake for something.
    //
    // `mode`, when set, is applied to the **tempfile** before persist
    // so the published path is always either nonexistent or fully
    // permission — a crash between `persist` and a later chmod
    // can't leave a stale non-executable bun on disk that we'd
    // happily reuse forever.
    let parent = dest.parent().ok_or_else(|| SidecarError::Io {
        path: dest.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "dest has no parent dir"),
    })?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|source| SidecarError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let mut decoder = zstd::stream::read::Decoder::new(zst)
        .map_err(|source| SidecarError::Decompress { what, source })?;
    io::copy(&mut decoder, tmp.as_file_mut())
        .map_err(|source| SidecarError::Decompress { what, source })?;
    tmp.as_file_mut()
        .sync_all()
        .map_err(|source| SidecarError::Io {
            path: tmp.path().to_path_buf(),
            source,
        })?;
    if let Some(mode) = mode {
        set_mode(tmp.path(), mode)?;
    }
    // `persist` does `rename(tmp, dest)` — atomic-replace on Unix.
    // If another gateway won the race and already published `dest`,
    // we still overwrite with byte-identical content and drop the
    // race-loser's tmp harmlessly.
    tmp.persist(dest).map_err(|e| SidecarError::Io {
        path: dest.to_path_buf(),
        source: e.error,
    })?;
    Ok(())
}

fn mkdir_all(dir: &Path) -> Result<(), SidecarError> {
    fs::create_dir_all(dir).map_err(|source| SidecarError::Io {
        path: dir.to_path_buf(),
        source,
    })
}

fn set_mode(path: &Path, mode: u32) -> Result<(), SidecarError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|source| SidecarError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Ensure `path`'s permission bits already match `mode` and, if not,
/// repair them. Used on the cached-runtime path so a partial install
/// from an older binary (or any out-of-band tampering) gets fixed up
/// the moment the gateway notices, instead of wedging the supervisor.
fn ensure_mode(path: &Path, mode: u32) -> Result<(), SidecarError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::metadata(path).map_err(|source| SidecarError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if meta.permissions().mode() & 0o777 == mode {
        return Ok(());
    }
    set_mode(path, mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn zst(bytes: &[u8]) -> &'static [u8] {
        Box::leak(zstd::bulk::compress(bytes, 1).unwrap().into_boxed_slice())
    }

    #[test]
    fn install_sidecar_places_aux_assets_next_to_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let aux_assets: &'static [SidecarAuxAsset] = Box::leak(
            vec![SidecarAuxAsset {
                name: "silk.wasm",
                content_zst: zst(b"wasm bytes"),
            }]
            .into_boxed_slice(),
        );
        let asset = SidecarAsset {
            channel_type: "weixin",
            bundle_zst: zst(b"console.log('weixin');"),
            content_hash: "fixture",
            aux_assets,
        };

        let bundle = install_sidecar(dir.path(), &asset).unwrap();

        assert_eq!(
            bundle.file_name().and_then(|s| s.to_str()),
            Some("index.js")
        );
        assert_eq!(fs::read(&bundle).unwrap(), b"console.log('weixin');");
        assert_eq!(
            fs::read(bundle.parent().unwrap().join("silk.wasm")).unwrap(),
            b"wasm bytes"
        );
    }

    #[test]
    fn safe_relative_path_rejects_escape_paths() {
        assert!(safe_relative_path("").is_err());
        assert!(safe_relative_path("../silk.wasm").is_err());
        assert!(safe_relative_path("/tmp/silk.wasm").is_err());
        assert!(safe_relative_path("silk.wasm").is_ok());
    }

    #[test]
    fn ensure_mode_repairs_non_executable_cached_bun() {
        // Simulates the partial-install crash path: a previous build
        // wrote the file but died before chmod, leaving it at 0o600.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bun");
        fs::write(&path, b"placeholder").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        ensure_mode(&path, 0o700).unwrap();

        let got = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(got, 0o700, "ensure_mode should restore 0o700");
    }

    #[test]
    fn ensure_mode_is_idempotent_on_already_correct_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bun");
        fs::write(&path, b"placeholder").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        ensure_mode(&path, 0o700).unwrap();
        let got = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(got, 0o700);
    }

    #[test]
    fn decompress_to_with_mode_publishes_file_already_executable() {
        // The fix for F2: the chmod must happen on the tempfile before
        // persist, so the published path is never observed without +x.
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("bun");
        let payload = b"#!/bin/sh\necho hi\n";
        let zst = zstd::bulk::compress(payload, 3).unwrap();

        decompress_to(&zst, &dest, "test runtime", Some(0o700)).unwrap();

        let mode = fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "published file must already be executable");
        assert_eq!(fs::read(&dest).unwrap(), payload);
    }

    #[test]
    fn decompress_to_without_mode_uses_umask_default() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("bundle.js");
        let payload = b"console.log('hi');";
        let zst = zstd::bulk::compress(payload, 3).unwrap();

        decompress_to(&zst, &dest, "test bundle", None).unwrap();

        // Whatever the umask is, no exec bit was forced — the JS
        // bundles aren't executable on disk.
        let mode = fs::metadata(&dest).unwrap().permissions().mode() & 0o111;
        assert_eq!(mode, 0, "data files should not get the exec bit");
        assert_eq!(fs::read(&dest).unwrap(), payload);
    }
}
