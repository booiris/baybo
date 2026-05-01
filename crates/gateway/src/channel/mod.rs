//! Gateway-side WebSocket channel server.
//!
//! Implements the server side of the wire protocol in
//! [`aura_channels::wire`]: a `/v1/channel-ws` endpoint mounted on
//! the channel TCP listener ([`crate::channel_listener`]), where
//! authenticated subprocess sidecars can register as dynamic
//! channels and exchange MessagePack-framed
//! messages with the agent. The only sidecar client is the TypeScript
//! package under `sdks/channel-ts/`; the built-in TUI has its own
//! private Rust WS client. Each accepted connection spawns a
//! per-connection [`adapter::Sidecar`] that plugs into the workspace
//! [`aura_channels::ChannelRegistry`] and tears itself down on
//! disconnect.
//!
//! Not currently exposed:
//! * **Streaming delta frames** — `AgentOutput::Delta` is coalesced into
//!   a single `Message` frame on the wire.

pub(crate) mod adapter;
pub(crate) mod blobs;
pub mod bot_reconciler;
pub mod control;
pub(crate) mod dedup;
pub mod diagnose;
pub(crate) mod handshake;
pub(crate) mod history;
pub mod mcp_tunnel;
pub mod route;
pub(crate) mod secrets;
pub(crate) mod session_resolver;
pub(crate) mod slash;
pub mod state;

pub use bot_reconciler::ChannelBotReconciler;
pub use control::{ChannelControlError, ChannelControlRegistry};
pub use dedup::InboundDedup;
pub use diagnose::{ChannelCapabilities, DiagnoseError, DiagnoseReport, DiagnoseRouter};
pub use history::TuiHistoryStore;
pub use mcp_tunnel::{McpTunnel, McpTunnelError, McpTunnelGuard, McpTunnelRouter, McpTunnelSender};
pub use route::routes;
pub use session_resolver::ChannelSessionResolver;
pub use state::WsChannelState;

/// Deterministic short hash of an identifier for log attribution.
/// Four hex chars distinguish concurrent pendings in a tracing log
/// without leaking the raw id.
pub(crate) fn short_hash(raw: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    raw.hash(&mut h);
    format!("{:04x}", (h.finish() & 0xFFFF) as u16)
}
