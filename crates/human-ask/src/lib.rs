//! The durable human-in-the-loop lifecycle: an ask is written to the
//! `human_asks` ledger before it is shown, parked on its channel's
//! [`AskQueue`], and resolved by a [`HumanAskManager`] verb that persists
//! first. Nothing times out. A dropped waiter takes its ask off the queue
//! without resolving it; the turn's cancel claim, or the next boot, closes
//! its row.
//!
//! See `docs/modules/human-ask.md`.

pub mod core;
pub mod error;
pub mod gate;
pub mod manager;
pub mod ports;
pub mod queue;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use crate::core::{AskCore, AskEvent};
pub use error::{HumanAskError, Result};
pub use gate::{AskChannels, ChannelAskGate, ChannelAsks, PolicyDenyGate};
pub use manager::{HumanAskManager, HumanAskManagerConfig};
pub use ports::{AskObserver, LivePendingAsks};
pub use queue::{
    AskQueue, AskQueueEvent, AskQueues, AskWatcher, PendingAsk, PendingEdge, PendingWatcher,
    ResolveFn,
};

#[cfg(test)]
mod tests;
