//! **relay** — the blind byte-pipe for `remote-host`.
//!
//! Solves "the phone can't reach the NAT'd gateway" and hosts pairing. Both the
//! pairing rendezvous (keyed by the public `rendezvous_id`) and the content relay
//! (keyed by the C-assigned `relay_node_id`) ride the same [`RelayBroker`]
//! primitive: match two legs by key, copy opaque frames blind. Pairing and
//! content run Noise above the splice, so C sees only routing keys and
//! ciphertext.
//!
//! The matching + piping core is the broker; [`serve`] layers the production
//! WebSocket transport on top (the `remote-host-relay` binary), hosting the
//! pairing rendezvous with admission so only an admitted gateway can occupy a
//! rendezvous.

pub mod bandwidth;
pub mod broker;
pub mod conns;
pub mod control;
pub mod error;
pub mod ip_limit;
pub mod serve;
pub mod ws;

pub use bandwidth::{BandwidthLimiter, BandwidthRegistry};
pub use broker::{RelayBroker, RelayLeg};
pub use conns::ConnectionRegistry;
pub use control::{ControlRegistry, ControlSignal};
pub use ws::pump_ws;
