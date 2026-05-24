//! Prompt framing the agent injects into the LLM transcript, consolidated so
//! every piece of model-facing framing has one home rather than being
//! scattered across the agent loop. See
//! `docs/todo/prompt-framing-to-context.md` for the migration plan.

pub mod cron;
pub mod subagent;
pub mod tool_output;
