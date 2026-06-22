//! Autonomous persistent objectives (`/goal`): the tool family, the
//! [`GoalService`] facade, and the verbatim continuation steering prompts.
//! This crate persists nothing itself — [`GoalService`] writes through an
//! `aura_store::GoalStore`; the continuation loop lives in `aura-agent`.
//! See `docs/modules/goal.md`.

pub mod prompts;
pub mod service;
pub mod tools;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use service::{GoalError, GoalService, PauseOutcome, ResumeOutcome};
pub use tools::agent_tools;
