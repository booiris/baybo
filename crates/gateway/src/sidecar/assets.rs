//! Embedded-asset extraction: turns the zstd-compressed sidecar
//! bundles baked in by `build.rs` into on-disk JS files the host's
//! `node` binary can run.
//!
//! Sidecars are spawned at runtime with the local `node` resolved
//! from `PATH` — no JS runtime is shipped inside the gateway binary.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use aura_workspace::paths::aura_cache_root;
use thiserror::Error;

mod generated {
    // Populated by `crates/gateway/build.rs` — see that file for
    // the emitted shape. When the sidecar pipeline degrades (pnpm
    // missing, esbuild bundling failed, etc.) the generated file
    // still compiles, just with `SIDECARS = &[]`.
    include!(concat!(env!("OUT_DIR"), "/sidecar_assets.rs"));
}

use generated::{SIDECARS, SidecarAsset, SidecarAuxAsset};

#[derive(Debug, Error)]
pub enum SidecarError {
    #[error("no embedded sidecars in this build (build.rs degraded); sidecars disabled")]
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

/// Materialised runtime: one JS path per embedded sidecar. Cheap to
/// hold (a small Vec); clone via `Arc` at the call site if shared
/// across tasks.
pub struct SidecarRuntime {
    sidecars: Vec<(String, PathBuf)>,
}

impl SidecarRuntime {
    /// Materialise every embedded sidecar bundle under the user cache
    /// dir. Idempotent: repeated calls notice the existing files and
    /// skip the zstd decode. If the build embeds no sidecars this
    /// returns [`SidecarError::RuntimeMissing`] — the caller is
    /// expected to log-and-skip rather than crash the gateway.
    pub fn install() -> Result<Self, SidecarError> {
        if SIDECARS.is_empty() {
            return Err(SidecarError::RuntimeMissing);
        }
        let cache_root = cache_root()?;
        let mut sidecars = Vec::with_capacity(SIDECARS.len());
        for asset in SIDECARS {
            let js_path = install_sidecar(&cache_root, asset)?;
            sidecars.push((asset.channel_type.to_string(), js_path));
        }
        Ok(Self { sidecars })
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

fn install_sidecar(cache_root: &Path, asset: &SidecarAsset) -> Result<PathBuf, SidecarError> {
    let dir = cache_root
        .join("sidecars")
        .join(format!("{}-{}", asset.channel_type, asset.content_hash));
    mkdir_all(&dir)?;
    // Hash-keyed: a bundle content change lands at a fresh filename
    // without ever rewriting a live one. Auxiliary files live next to
    // bundle.mjs so packages that resolve assets from import.meta.url
    // can find them.
    let dest = dir.join("bundle.mjs");
    if !dest.exists() {
        decompress_to(asset.bundle_zst, &dest, "sidecar bundle")?;
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
    decompress_to(asset.content_zst, &dest, "sidecar aux asset")
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

fn decompress_to(zst: &[u8], dest: &Path, what: &'static str) -> Result<(), SidecarError> {
    // Per-process tempfile under the same dir as `dest`, atomically
    // persisted. `$XDG_CACHE_HOME/aura/` is user-level shared across
    // every workspace the same UID runs, so two gateways
    // first-installing concurrently would otherwise race on a fixed
    // `<dest>.part`. NamedTempFile gives each a unique `.tmp<rand>`
    // and the second `persist` atomically replaces the first's target
    // (contents are byte-identical — same embedded blob — so either
    // winner is fine). A crash mid-write also leaves the
    // NamedTempFile's `Drop` unlinking the partial.
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
            Some("bundle.mjs")
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
    fn decompress_to_uses_umask_default() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("bundle.mjs");
        let payload = b"console.log('hi');";
        let zst = zstd::bulk::compress(payload, 3).unwrap();

        decompress_to(&zst, &dest, "test bundle").unwrap();

        let mode = fs::metadata(&dest).unwrap().permissions().mode() & 0o111;
        assert_eq!(mode, 0, "data files should not get the exec bit");
        assert_eq!(fs::read(&dest).unwrap(), payload);
    }
}
