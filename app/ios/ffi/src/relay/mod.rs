//! Relay (scan-to-pair) gateway access — the Noise E2E path.
//!
//! Mirrors [`crate::direct`]: a self-contained transport leg grouping its own
//! auth, chat, and attachment code. [`pairing`] runs the QR scan-to-pair
//! handshake + persisted pairing record; [`chat`] is the relay chat leg (Noise
//! over the relay's content-join leg); [`blob`] is the relay attachment leg. The
//! shared frame pump lives in [`crate::transport`]; this path only supplies its
//! relay-specific establish + codec seams.

mod api;
mod blob;
mod chat;
mod dial;
pub(crate) mod leg_pool;
mod pairing;
mod tunnel;

pub(crate) use api::GatewayApi;
pub(crate) use chat::RelaySessions;
pub(crate) use pairing::{
    PairingSessions, forget_pairing, has_pairing, pair_begin, pair_confirm, paired_device,
};
