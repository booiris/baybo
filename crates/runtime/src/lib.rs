//! `aura-runtime` — the reusable boot + manager-graph + router wiring that
//! stands up an Aura gateway, extracted from the `aura` binary crate so it
//! can be consumed by both the CLI and the macOS app (which embeds the
//! runtime in-process). See `docs/mac-app.md` §3.
//!
//! This crate owns the *application boot layer*: config loading, the LLM
//! client/pool construction, the full `ManagerGraph`, router wiring, the
//! config hot-reload orchestrator, and the per-workspace singleton lock.
//! It deliberately does **not** own process-global concerns — tracing
//! install, signal handling beyond the opt-in helpers, and the CLI banner
//! stay with the host (`src/` for the CLI, the Tauri app for the desktop
//! build).

pub mod boot;
pub mod reload;
pub mod runtime;
pub mod singleton;
pub mod start;

pub use crate::start::{
    RunningGateway, SetupTracing, StartGatewayOpts, TracingGuard, start_gateway,
};
