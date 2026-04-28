use serde::{Deserialize, Serialize};

use crate::JobStatus;

/// Identifies what a `TraceNode` recorded.
///
/// Span/node-level granularity: one variant per atomic action that
/// happens inside a turn (LLM call, tool call, sub-agent spawn, …).
/// Job-level "what initiated this turn" lives on `JobKind` — the two
/// enums are not yet fully separated (see `Job::kind` doc) but
/// callers writing trace nodes should reach for `OperationKind`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OperationKind {
    LlmCall {
        model: String,
    },
    ToolExecution {
        tool_name: String,
    },
    SkillExecution {
        skill_name: String,
    },
    CronExecution {
        cron_job_id: String,
    },
    ContextCompression {
        strategy: String,
    },
    MemoryOperation {
        operation: String,
    },
    UserMessageHandling {
        session_id: String,
    },
    /// Parent span dispatched a sub-agent into a fresh child session
    /// and is waiting for it to terminate.
    SubAgentSpawn {
        child_session_id: String,
        child_job_id: String,
    },
    /// JobManager moved a job through the acceptance chain. Recorded
    /// as a span so the trace timeline shows when a verifier signed
    /// off (or rejected) the agent's output.
    Acceptance {
        from: JobStatus,
        to: JobStatus,
    },
}

/// Identifies what initiated a `Job` — turn-level granularity.
///
/// Currently `Job::kind` still holds an `OperationKind` for backwards
/// compatibility (one-Job-per-operation legacy). A future refactor
/// will switch `Job::kind` to `JobKind` and drop the per-operation
/// jobs in favour of a single turn-level job per actor message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobKind {
    /// A user-typed turn (chat input or `/<command>` invocation).
    UserMessage,
    /// A cron schedule fired. `cron_job_id` is the persistent CronJob
    /// id; the per-fire `CronExecution` lives in storage and is
    /// linked separately via `cron_executions.job_id`.
    CronExecution { cron_job_id: String },
    /// Aura itself initiated this turn (periodic review, compaction,
    /// memory consolidation, skill discovery, …).
    SystemAction { trigger: String },
    /// Parent agent spawned a sub-agent. `tool_call_id` is the parent
    /// LLM's tool-call id that triggered the spawn — useful for
    /// correlating the parent's `OperationKind::SubAgentSpawn` span
    /// with the child's owning Job.
    SubAgentDelegation { tool_call_id: String },
}
