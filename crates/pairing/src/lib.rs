//! Per-user pairing gate for channel sidecars.
//!
//! Sidecars forward every inbound user message to baybo. Before baybo
//! admits a message into the agent loop it consults
//! [`PairingService::check`], which looks up the `(channel_type,
//! bot_id, user_id)` triple. Unknown or expired-pending senders get a
//! short human-typable code back; an operator runs
//! `baybo pair approve <code>` and the next message from the same
//! triple flows through unimpeded.
//!
//! See `docs/modules/pairing.md` for the full design.

mod code;
mod device_service;
mod device_slot;
mod error;
mod service;

pub use code::{CODE_LEN, CodeError, generate_code};
pub use device_service::DevicePairingService;
pub use device_slot::DevicePairingSlot;
pub use error::{DevicePairingError, PairingError};
pub use service::{CheckOutcome, PairingService};
