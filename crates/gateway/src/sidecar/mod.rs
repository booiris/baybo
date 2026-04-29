//! Embedded channel-sidecar runtime.
//!
//! The gateway ships each in-tree sidecar (`channel-src/*`) as a
//! zstd-compressed JS bundle, baked in at build time by `build.rs`.
//! Sidecars run on the host's `node` binary (resolved from `PATH`) —
//! no JS runtime is shipped inside the gateway binary. At boot the
//! gateway materialises every embedded bundle to the user's cache
//! directory (once per bundle hash) and hands [`ChannelSpawner`] a
//! `Command` so every sidecar runs as a supervised subprocess.
//!
//! Layout on disk (`$XDG_CACHE_HOME/aura/` or `~/.cache/aura/`):
//!
//! ```text
//! sidecars/<channel>-<hash>/bundle.mjs   # plain ESM, run by node
//! sidecars/<channel>-<hash>/<aux...>     # any aux assets (e.g. silk.wasm)
//! ```
//!
//! Hash-keyed paths mean an aura upgrade lands a fresh bundle without
//! touching the old ones — intentional: a crashed or downgraded
//! install can still fall back to whatever was there before.
//!
//! [`SidecarSupervisor`] wraps [`ChannelSpawner`] with a restart loop
//! per channel type, backoff-capped, driven by the shared shutdown
//! signal.

mod assets;
mod supervisor;

pub use assets::{SidecarError, SidecarRuntime};
pub use supervisor::{NODE_BINARY_ENV, SidecarSupervisor};
