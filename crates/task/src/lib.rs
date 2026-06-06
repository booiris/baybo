//! The session planning-checklist tools (`Task*`).
//!
//! Mirrors `aura-cron` / `aura-skills` / `aura-subagent`: a domain crate that
//! owns its own `Tool` impls and depends on `aura-tools` for the trait, never
//! the reverse. The tools are thin, store-scoped CRUD over the per-session
//! checklist behind `aura_store::TaskStore` — no actor round-trip, because the
//! `session_tasks` table is shared state (per-row writes are atomic), not the
//! actor-owned `SessionState` blob.
//!
//! Modeled on Claude Code's `Task*` tools and Codex's `update_plan`: a small
//! status enum (`pending -> in_progress -> completed`), an
//! at-most-one-`in_progress` convention surfaced in the descriptions, and the
//! list re-injected into the model's context as a throttled periodic reminder
//! (the agent loop owns that; see `docs/modules/agent.md`).

pub mod tools;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use tools::agent_tools;
