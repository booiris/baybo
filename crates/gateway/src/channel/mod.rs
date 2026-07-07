//! Gateway-side WebSocket channel server.
//!
//! Implements the server side of the wire protocol in
//! [`baybo_channels::wire`]: a `/v1/channel-ws` endpoint mounted on
//! the channel TCP listener ([`crate::channel_listener`]), where
//! authenticated subprocess sidecars can register as dynamic
//! channels and exchange MessagePack-framed
//! messages with the agent. The only sidecar client is the TypeScript
//! package under `sidecars/sdk/channel-ts/`; the built-in TUI has its own
//! private Rust WS client. Each accepted connection spawns a
//! per-connection [`adapter::Sidecar`] that plugs into the workspace
//! [`baybo_channels::ChannelRegistry`] and tears itself down on
//! disconnect.
//!
//! Streaming: every `SessionEvent` (including `AgentEvent::AnswerDelta`,
//! `Reasoning`, and the `Tool*` progress events) is translated 1:1 to its
//! wire `Frame` and sent live — `translator_loop` does no coalescing.
//! Clients without a partial surface (multiplexed sidecars) simply ignore
//! the streaming frames and render the final `Message`.

pub(crate) mod adapter;
pub(crate) mod api_tunnel;
pub(crate) mod blobs;
pub mod boot;
pub mod bot_reconciler;
pub mod control;
pub(crate) mod dedup;
pub(crate) mod device_content;
pub(crate) mod device_pair;
pub(crate) mod handshake;
pub(crate) mod history;
pub(crate) mod relay_content;
#[cfg(test)]
mod relay_e2e;
pub(crate) mod relay_pair;
pub mod route;
pub(crate) mod session_pulse;
pub(crate) mod session_resolver;
pub(crate) mod slash;
pub mod state;
pub(crate) mod tunnel_http;
pub(crate) mod work_steps;

pub use bot_reconciler::ChannelBotReconciler;
pub use control::{ChannelControlError, ChannelControlRegistry};
pub use dedup::InboundDedup;
pub use history::TuiHistoryStore;
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
