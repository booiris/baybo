//! **relay** — the blind byte-pipe + SPAKE2 rendezvous for `aura-remote-host`.
//!
//! Solves "the phone can't reach the NAT'd gateway" and hosts pairing. Both the
//! pairing rendezvous (keyed by the SPAKE2 code) and the content relay (keyed by
//! the C-assigned `relay_node_id`) ride the same [`RelayBroker`] primitive:
//! match two legs by key, copy opaque frames blind. Pairing runs SPAKE2 and
//! content runs Noise inside TLS, so C sees neither the code nor any plaintext —
//! it only shuttles ciphertext.
//!
//! This crate is the matching + piping core; the production WebSocket transport
//! (accepting two sockets and pumping their binary frames through a
//! [`RelayLeg`]) layers on top.

pub mod broker;
pub mod control;

pub use broker::{RelayBroker, RelayLeg};
pub use control::{ControlRegistry, ControlSignal};
