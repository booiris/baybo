//! Human-in-the-loop prompts: the per-channel [`AskQueue`] of parked
//! approval prompts, the [`ChannelAskGate`] that parks a tool call on it,
//! and the [`PolicyDenyGate`] unattended turns are routed to.
//!
//! `baybo-tools` owns the tool-facing ports (`ApprovalGate`,
//! `ApprovalGateMap`, `ApprovalHandle`); this crate implements them and never
//! names a channel. See `docs/modules/human-ask.md`.

pub mod gate;
pub mod queue;

pub use gate::{ChannelAskGate, PolicyDenyGate};
pub use queue::{AskQueue, PendingEdge, PendingWatcher, ResolveFn};
