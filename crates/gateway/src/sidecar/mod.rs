//! Embedded channel-sidecar runtime.
//!
//! The gateway ships each in-tree sidecar (`sidecars/channel/*`) as a
//! zstd-compressed JS bundle, baked in at build time by `build.rs`.
//! Sidecars run on the host's `bun` binary (located by
//! [`baybo_process::HostTool`]) — no JS runtime is shipped
//! inside the gateway binary. At boot the gateway materialises every
//! embedded bundle to the user's cache directory (once per bundle
//! hash) and hands [`ChannelSpawner`] a `Command` so every sidecar
//! runs as a supervised subprocess.
//!
//! Layout on disk (`$XDG_CACHE_HOME/baybo/` or `~/.cache/baybo/`):
//!
//! ```text
//! sidecars/<channel>-<hash>/bundle.mjs   # bun-built, run by bun
//! sidecars/<channel>-<hash>/<aux...>     # any aux assets (e.g. silk.wasm)
//! ```
//!
//! Hash-keyed paths mean an baybo upgrade lands a fresh bundle without
//! touching the old ones — intentional: a crashed or downgraded
//! install can still fall back to whatever was there before.
//!
//! [`SidecarSupervisor`] wraps [`ChannelSpawner`] with a restart loop
//! per channel type, backoff-capped, driven by the shared shutdown
//! signal.

mod assets;
mod embedded_mcp;
pub(crate) mod pipe_pump;
mod supervisor;

pub use assets::{SidecarError, SidecarRuntime, domains};
pub use embedded_mcp::collect_profiles;
pub use supervisor::SidecarSupervisor;
