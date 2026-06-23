//! **aura-mobile-core** — the FFI-free shared client core for the Aura iOS
//! companion (and a future Android app).
//!
//! It runs in the Tauri shell's `src-tauri` (reached from the webview via Tauri
//! commands) and speaks the exact same protocol as the gateway by depending on
//! the same crates — [`aura_wire`] for the `Frame` wire types and
//! [`aura_device_proto`] for SPAKE2 / Noise / the AEAD framing. No FFI, no
//! Tauri, no platform APIs here, so it is fully host-unit-testable and
//! cross-compiles to `aarch64-apple-ios` unchanged.
//!
//! This first slice is the app side of the pairing handshake ([`pairing`]); the
//! Noise content session + `Frame::Subscribe` self-pull layer follow.

pub mod connect;
pub mod content;
pub mod error;
pub mod pairing;

pub use connect::{ConnectError, Endpoint, connect_first, connection_plan};
pub use content::{ContentHandshake, ContentSession};
pub use error::MobileError;
pub use pairing::{PairedGateway, PairingClient, PairingRequest};

// Re-export the wire `Frame` so the shell renders threads against the same type
// the gateway emits.
pub use aura_wire::Frame;
