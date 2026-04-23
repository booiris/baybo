//! Embedded channel-sidecar runtime.
//!
//! The gateway ships each in-tree sidecar (`channel-src/*`) as a
//! zstd-compressed JS bundle and a single shared zstd-compressed `bun`
//! runtime, both baked in at build time by `build.rs`. At boot the
//! gateway materialises them to the user's cache directory (once per
//! bun version / bundle hash) and hands [`ChannelSpawner`] a `Command`
//! so every sidecar runs as a supervised subprocess.
//!
//! Layout on disk (`$XDG_CACHE_HOME/aura/` or `~/.cache/aura/`):
//!
//! ```text
//! runtime/bun-<ver>-<target>        # executable, chmod 0700
//! sidecars/<channel>-<hash>.js      # plain JS, read by bun
//! ```
//!
//! Version- and hash-keyed paths mean an aura upgrade lands a fresh
//! pair of files without touching the old ones — intentional: a
//! crashed or downgraded install can still fall back to whatever was
//! there before.
//!
//! [`SidecarSupervisor`] wraps [`ChannelSpawner`] with a restart loop
//! per channel type, backoff-capped, driven by the shared shutdown
//! signal.

mod assets;
mod supervisor;

pub use assets::{SidecarError, SidecarRuntime};
pub use supervisor::SidecarSupervisor;

/// Pinned bun version baked into this build. Surfaced for diagnostics
/// (boot log, `/v1/status`). Returns `"unknown"` when `.bun-version`
/// was unreadable at build time.
pub fn bun_version() -> &'static str {
    assets::BUN_VERSION
}
