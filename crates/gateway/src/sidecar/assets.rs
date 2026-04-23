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
use std::path::{Path, PathBuf};

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
use generated::{BUN_RUNTIME_ZST, BUN_TARGET, SIDECARS, SidecarAsset};

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
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .ok_or(SidecarError::NoCacheDir)?;
    Ok(base.join("aura"))
}

fn install_bun(cache_root: &Path) -> Result<PathBuf, SidecarError> {
    let dir = cache_root.join("runtime");
    mkdir_all(&dir)?;
    // Version AND target are in the filename so a sibling install on
    // a different target (bind-mounted home dir, etc.) doesn't trip
    // over ours.
    let dest = dir.join(format!("bun-{BUN_VERSION}-{BUN_TARGET}"));
    if dest.exists() {
        return Ok(dest);
    }
    decompress_to(BUN_RUNTIME_ZST, &dest, "bun runtime")?;
    set_executable(&dest)?;
    Ok(dest)
}

fn install_sidecar(cache_root: &Path, asset: &SidecarAsset) -> Result<PathBuf, SidecarError> {
    let dir = cache_root.join("sidecars");
    mkdir_all(&dir)?;
    // Hash-keyed: a bundle content change lands at a fresh filename
    // without ever rewriting a live one. Old hashes linger on disk by
    // design (see the module comment's "don't delete old" note).
    let dest = dir.join(format!("{}-{}.js", asset.channel_type, asset.content_hash));
    if dest.exists() {
        return Ok(dest);
    }
    decompress_to(asset.bundle_zst, &dest, "sidecar bundle")?;
    Ok(dest)
}

fn decompress_to(zst: &[u8], dest: &Path, what: &'static str) -> Result<(), SidecarError> {
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

fn set_executable(path: &Path) -> Result<(), SidecarError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)
        .map_err(|source| SidecarError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .permissions();
    perms.set_mode(0o700);
    fs::set_permissions(path, perms).map_err(|source| SidecarError::Io {
        path: path.to_path_buf(),
        source,
    })
}
