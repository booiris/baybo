//! `bench-web` — a standalone, read-only viewer for the on-disk
//! artifacts the bench harnesses (`bench/swe`, `bench/terminal-bench-*`,
//! `bench/memory`) leave under their `results/` and `trace/` dirs.
//!
//! It scans the filesystem fresh on every request (completed runs only),
//! normalizes each bench's heterogeneous `results-*.json` into one shared
//! [`model`] spine via per-bench [`adapters`], and serves both a small
//! JSON API and the embedded React bundle. No database, no auth — it
//! reads files a developer already has locally.

pub mod adapters;
pub mod api;
pub mod model;
pub mod precompute;

mod error;
mod input;
mod trace;
mod webui;
