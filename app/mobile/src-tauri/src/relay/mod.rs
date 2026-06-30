//! Relay (scan-to-pair) gateway access — the Noise E2E path.
//!
//! Mirrors [`crate::direct`]: a self-contained transport leg grouping its own
//! auth, chat, and attachment code. [`pairing`] runs the QR scan-to-pair
//! handshake + persisted pairing record; [`chat`] is the relay chat leg (Noise
//! over the relay's content-join leg); [`blob`] is the relay attachment leg. The
//! shared frame pump lives in [`crate::transport`]; this path only supplies its
//! relay-specific establish + codec seams.

mod blob;
mod chat;
mod pairing;

pub use blob::{download, image_data, upload, upload_bytes};
pub use chat::{RelaySessions, connect, disconnect, send};
pub use pairing::{
    PairAborted, PairChallenge, PairedSummary, PairingSessions, forget_pairing, pair_begin,
    pair_confirm, paired_device,
};
