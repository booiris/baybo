use serde::{Deserialize, Serialize};

/// Shared operation type used by both `job` and `trace` to identify the kind of work.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OperationKind {
    LlmCall { model: String },
    ToolExecution { tool_name: String },
    SkillExecution { skill_name: String },
    CronExecution { cron_job_id: String },
    ContextCompression { strategy: String },
    MemoryOperation { operation: String },
    UserMessageHandling { session_id: String },
}
