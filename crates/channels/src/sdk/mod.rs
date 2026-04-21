//! WebSocket + MessagePack SDK for Rust channel sidecars.
//!
//! Exposes [`Client`], [`Message`], [`Frame`]. Call
//! [`Client::connect`] to read `AURA_CHANNEL_SOCKET` /
//! `AURA_CHANNEL_TOKEN` from the environment, dial the UDS the
//! gateway injected, and run the Register/RegisterAck handshake.
//!
//! TypeScript / JavaScript sidecars consume the same wire protocol
//! through the hand-written `sdks/channel-ts/` npm package, which
//! reuses ts-rs-generated type bindings from [`wire`].
//!
//! Authentication rides the first WS *message* (the `Register` frame),
//! not an HTTP upgrade header — that choice is deliberate, because
//! browser / Node `WebSocket` APIs don't allow setting custom headers
//! on the handshake. The same protocol therefore works on every target.

pub mod client;
pub mod error;
pub mod wire;

pub use client::{Client, ENV_CHANNEL_SOCKET, ENV_CHANNEL_TOKEN};
pub use error::SdkError;
pub use wire::{Frame, Message, PROTOCOL_VERSION, decode, encode};

#[cfg(test)]
mod tests;
