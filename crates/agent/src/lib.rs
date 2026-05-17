//! `aura-agent` is the assembly layer for the Aura execution engine.
//! The agent's two halves are organised into sibling subdirectories:
//!
//! - [`runtime`] holds per-turn execution (agent loop, tool executor,
//!   compression wiring, billed-chat helper, soul/system-prompt
//!   builder, session log, sandbox + scope guards, LLM client pool).
//! - [`actor`] holds the per-session actor (mailbox, supervisor, the
//!   message router, subagent wait routine, and `DurableActorState`).
//!
//! `security` and `service` stay at the top level: `SecurityGateway`
//! is the cross-cutting interception facade threaded through both
//! halves, and `service` is process-level shutdown / task-tracker
//! infrastructure that doesn't belong to either half.

pub mod actor;
pub mod cron_tools;
pub mod runtime;
pub mod security;
pub mod service;

// Stable external paths — consumers import `aura_agent::agent_loop::*`,
// `aura_agent::supervisor::*`, etc. The submodules live under `runtime`
// and `actor` after the PR6 reorg; these re-exports keep the wire shape.
pub use actor::router;
pub use actor::state;
pub use actor::subagent;
pub use actor::supervisor;
pub use runtime::agent_loop;
pub use runtime::billed_chat;
pub use runtime::compression;
pub use runtime::error_recovery;
pub use runtime::llm_pool;
pub use runtime::sandbox;
pub use runtime::session_log;
pub use runtime::soul;
pub use runtime::tool_executor;

pub use agent_loop::AgentLoop;
pub use aura_cron::{CronScheduler, CronTriggerEvent};
pub use aura_security::SecretVault;
pub use aura_session::SessionManager;
pub use billed_chat::BilledChatFactory;
pub use llm_pool::LlmClientPool;
pub use router::{ActorSpawner, Router};
pub use security::{LeakRuleSummary, SecretVaultSummary, SecurityAuditReport, SecurityGateway};
pub use service::{ShutdownSignal, TaskTracker};
pub use session_log::{
    LlmCallOutcome, LlmCallRecord, LlmRequestMeta, LlmResponseMeta, SessionLlmLogger,
    SessionMessageRecord,
};
pub use supervisor::AgentSupervisor;
pub use tool_executor::ToolExecutor;
