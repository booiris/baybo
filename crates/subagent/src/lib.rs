//! Typed subagent profile definitions and registry.
//!
//! A `SubagentProfile` declares the system prompt + default model tier
//! for a single `subagent_type` value the parent LLM can emit when
//! calling `spawn_subagent`. The profile fully replaces the parent's
//! Soul for the spawned child actor — the profile author owns the
//! identity, security, and output contracts of that child.
//!
//! Disk layout: one `<name>.md` per profile under
//! `<workspace>/agents/`. Frontmatter sets discovery / version /
//! default tier; the body is the system prompt.
//!
//! This crate owns the subagent domain end to end: profile
//! definitions, the registry, the fan-out dispatch limiter, and the
//! `spawn_subagent` tool itself. Like `aura-cron` and `aura-skills`, it
//! depends on `aura-tools` for the `Tool` trait rather than letting its
//! tool live in `aura-tools` — a domain crate owns its own tools, and
//! `aura-tools` must not depend back on one (that would be a cycle).

mod builtin;
mod dispatch;
mod loader;
mod profile;
mod registry;
pub mod tool;
mod validation;

pub use dispatch::{FanOutLimiter, SubagentDispatchLimiter, unbounded_limiter};
pub use loader::{load_profile_from_file, parse_profile_md};
pub use profile::{SubagentProfile, SubagentProfileSummary};
pub use registry::SubagentRegistry;
pub use validation::{normalize_line_endings, validate_profile_name, validate_profile_version};
