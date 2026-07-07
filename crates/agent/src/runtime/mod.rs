//! Execution core for one turn of the agent loop.
//!
//! Owns the LLM call → tool dispatch → response assembly pipeline plus
//! the helpers that bind LLM/cost/security/storage together for that
//! pipeline. Per-turn lifecycle scopes live in [`scope`]; the
//! cost-aware LLM call wrapper is [`billed_chat`]; the maintenance
//! companion to the inline compression flow is [`compression`].

pub mod agent_loop;
pub mod background_jobs;
pub mod billed_chat;
pub mod compression;
pub mod error_recovery;
pub mod llm_pool;
pub mod progress_observer;
pub mod sandbox;
pub mod scope;
pub mod subagent_spawner;
pub mod title;
pub mod tool_executor;
pub mod virtual_read;
