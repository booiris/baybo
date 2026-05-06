//! Memory & skill extraction side-channel — see
//! `docs/modules/self-improvement.md` for the design.
//!
//! `SelfImprovementManager` (in `manager.rs`) subscribes to
//! [`crate::JobLifecycle::subscribe_terminal_events`], filters terminal
//! events to "complex completed user-chat", applies the daily cap and
//! per-user / global concurrency limits, and dispatches a
//! `SystemTriggerEvent` into the router's mpsc — Router then mints a
//! fresh `TriggerSource::System { reason: SelfImprovement }` session and
//! runs a self_improvement `JobKind::System` job in it.
//!
//! `tools.rs` defines the four tools (`MemoryWrite`, `MemoryList`,
//! `SkillCreate`, `SkillList`) the self_improvement agent runs against.
//! They are NEVER added to a user-facing agent's `allowed_tools`; the
//! protection model relies on this isolation (per Q7 of the grilling
//! session — empty `accessed_resources()` bypasses the approval gate
//! safely only because the tools are not exposed to channel agents).

pub mod manager;
pub mod prompt;
pub mod tools;

pub use manager::{SelfImprovementConfig, SelfImprovementManager, SystemTriggerEvent};
pub use tools::self_improvement_tools;
