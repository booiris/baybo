//! Prompt framing the agent injects into the LLM transcript, consolidated so
//! every piece of model-facing framing has one home rather than being
//! scattered across the agent loop. See
//! `docs/todo/prompt-framing-to-context.md` for the migration plan.

pub mod cancelled_turn;
pub mod compression;
pub mod cron;
pub mod interjection;
pub mod recalled_memory;
pub mod soul;
pub mod subagent;
pub mod tasks;
pub mod title;
pub mod tool_output;
