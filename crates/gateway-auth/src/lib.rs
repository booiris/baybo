//! Authentication primitives for the gateway's channel UDS listener.
//!
//! This crate exists separately from `aura-gateway` so both the gateway
//! server and the TUI client can link against the same compiled PSK
//! without the TUI pulling in the entire gateway crate graph.
//!
//! Two identity mechanisms are exposed:
//!
//! * [`effective_tui_psk`] derives a stable per-install 32-byte secret
//!   from an embedded PSK plus a workspace-local salt file. The TUI
//!   sends this as a header when connecting to the channel UDS, and the
//!   gateway compares constant-time against the value it derives from
//!   the same inputs.
//!
//! * [`ChannelTokenTable`] manages one-shot tokens handed out when the
//!   gateway spawns a channel sidecar as a subprocess. The subprocess
//!   presents the token on each connection; the gateway also verifies
//!   that the connecting peer's PID matches the child it was issued to.

pub mod psk;
pub mod token;

pub use psk::{effective_tui_psk, EMBEDDED_TUI_PSK, TUI_PSK_HEADER};
pub use token::{
    constant_time_eq, ChannelTokenError, ChannelTokenTable, ClientIdentity, TokenHandle,
    CHANNEL_TOKEN_HEADER,
};
