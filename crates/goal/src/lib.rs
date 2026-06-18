//! Autonomous persistent objectives (`/goal`): the tool family, the
//! [`GoalService`] facade, and the verbatim continuation steering prompts.
//!
//! Mirrors `aura-task` / `aura-cron`: a domain crate that owns its own `Tool`
//! impls over a `*Store` trait and depends on `aura-tools` for the trait, never
//! the reverse. It persists nothing itself — [`GoalService`] writes through an
//! `aura_store::GoalStore`. The architecturally invasive part (the turn-boundary
//! continuation loop, accounting, failure handling) lives in `aura-agent`, which
//! consumes [`GoalService`]. See `docs/modules/goal.md`.

pub mod prompts;
pub mod service;
pub mod tools;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use service::{GoalError, GoalService};
pub use tools::agent_tools;
