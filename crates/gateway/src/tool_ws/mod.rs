//! Tool-sidecar WS subsystem. The browser tool drives a Playwright
//! process running inside a bundled bun sidecar; communication happens
//! over a dedicated `/v1/tool-ws` endpoint with a JSON-RPC-style
//! protocol carried in msgpack frames (see `frame.rs`).
//!
//! Layering:
//! - [`frame::ToolFrame`] is the wire type.
//! - [`server::routes`] mounts the axum WS handler and runs the
//!   register handshake.
//! - [`client::WsBrowserSidecarClient`] is the `aura_tools` trait
//!   implementation tools call into.
//! - [`supervisor::ToolSidecarSupervisor`] drives the bun process'
//!   restart loop (skipped when the operator wires an external
//!   sidecar).

pub mod client;
pub mod frame;
pub mod server;
pub mod supervisor;

pub use client::WsBrowserSidecarClient;
pub use server::{ToolWsState, routes};
pub use supervisor::{ToolSidecarConfig, ToolSidecarSupervisor};
