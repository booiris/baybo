//! Gateway-side WebSocket channel server.
//!
//! Implements the server half of the sidecar SDK shipped in
//! `aura_channels::sdk`: a `/v1/channel-ws` endpoint mounted on the
//! channel UDS listener, where authenticated subprocess sidecars can
//! register as dynamic channels and exchange MessagePack-framed
//! messages with the agent. Each accepted connection spawns a
//! per-connection [`adapter::SidecarAdapter`] that plugs into the
//! workspace [`aura_channels::ChannelRegistry`] and tears itself down
//! on disconnect.
//!
//! Not currently exposed:
//! * **Streaming delta frames** — `AgentOutput::Delta` is coalesced into
//!   a single `Message` frame on the wire.
//! * **Sidecar-driven tool approval** — `approval_gate()` is `None`.

pub(crate) mod adapter;
pub(crate) mod handshake;
pub mod route;
pub mod state;

pub use route::routes;
pub use state::WsChannelState;
