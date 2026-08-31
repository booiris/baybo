//! Prompt framing the agent injects into the LLM transcript, consolidated so
//! every piece of model-facing framing has one home rather than being
//! scattered across the agent loop.

pub mod background_notification;
pub mod cancelled_turn;
pub mod compression;
pub mod cron;
pub mod deferred_tools;
pub mod interjection;
pub mod issue;
pub mod line_diff;
pub mod no_progress;
pub mod recalled_memory;
pub mod skills_update;
pub mod soul;
pub mod system_prompt_update;
pub mod tasks;
pub mod title;
pub mod tool_output;
