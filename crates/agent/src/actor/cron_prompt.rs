//! Cron-fire prompt framing moved to [`aura_context::prompts::cron`]; this
//! module re-exports it so existing `aura_agent::cron_prompt::*` paths — the
//! gateway admin chat panel and the e2e tests — keep resolving. The agent
//! actor itself frames + appends a fire via `AgentLoop::append_cron_fire`.

pub use aura_context::prompts::cron::{frame_cron_prompt, original_cron_prompt};
