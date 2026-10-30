//! The durable human-in-the-loop lifecycle: an ask is written to the
//! `human_asks` ledger before it is shown, parked on its channel's
//! [`AskQueue`], and resolved by a [`HumanAskManager`] verb that persists
//! first. Nothing times out; a dropped waiter orphans its ask instead of
//! resolving it, so asks survive a turn cancel by the process and a restart.
//!
//! See `docs/modules/human-ask.md`.

pub mod core;
pub mod error;
pub mod gate;
pub mod manager;
pub mod ports;
pub mod queue;
pub mod result;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use crate::core::{AskCore, AskEvent};
pub use error::{HumanAskError, Result};
pub use gate::{ChannelAskGate, PolicyDenyGate};
pub use manager::{HumanAskManager, HumanAskManagerConfig, ReloadReport, Settled, ToolCallAbandon};
pub use ports::{AskObserver, LivePendingAsks, ResumeSink};
pub use queue::{
    AskQueue, AskQueueEvent, AskQueues, AskWatcher, PendingAsk, PendingEdge, PendingWatcher,
    ResolveFn,
};

#[cfg(test)]
mod tests;
