use std::time::Duration;

use aura_sandbox::SandboxError;
use aura_tools::ToolError;
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum CodeBuilderError {
    /// Wraps the sanitised error string returned by
    /// [`aura_llm::BilledChat::chat`]. The agent-side billed adapter
    /// has already scrubbed any leaked secrets out of the message, so
    /// it's safe to surface verbatim through `ToolError`.
    #[error("LLM call failed: {0}")]
    LlmFailed(String),

    /// `ToolContext.llm` was `None` — no per-actor `BilledChatFactory`
    /// was bound for this execute call. CodeBuilder fails closed
    /// rather than silently falling back to a process default that
    /// could bypass the session's pinned LLM (and its credentials /
    /// pricing / trust profile).
    #[error("CodeBuilder requires a billed LLM handle on the tool context; none was bound")]
    LlmUnavailable,

    #[error("LLM returned non-JSON: {preview}")]
    LlmReturnedNoJson { preview: String },

    #[error("LLM plan failed schema validation: {0}")]
    LlmPlanInvalid(#[from] serde_json::Error),

    #[error("LLM plan rejected: {0}")]
    LlmPlanRejected(String),

    #[error("OS sandbox unavailable: {0}")]
    SandboxUnavailable(String),

    #[error("`uv` not found on PATH; install from https://astral.sh/uv")]
    UvNotFound,

    #[error("sandbox cannot enforce requested resource caps: {0}")]
    Unenforceable(String),

    #[error("sandbox run failed: {0}")]
    SandboxRunFailed(String),

    #[error("CodeBuilder exceeded {0:?}")]
    RunTimeout(Duration),

    #[error("cancelled")]
    Cancelled,

    #[error("scratch dir setup failed: {0}")]
    Scratch(String),
}

impl From<SandboxError> for CodeBuilderError {
    fn from(e: SandboxError) -> Self {
        match e {
            SandboxError::Timeout(d) => CodeBuilderError::RunTimeout(d),
            SandboxError::Unenforceable { .. } => CodeBuilderError::Unenforceable(e.to_string()),
            SandboxError::BackendMissing { .. }
            | SandboxError::BackendUnreachable { .. }
            | SandboxError::NoBackendAvailable
            | SandboxError::BackendNotExecutable { .. }
            | SandboxError::UnsupportedPlatform(_) => {
                CodeBuilderError::SandboxUnavailable(e.to_string())
            }
            other => CodeBuilderError::SandboxRunFailed(other.to_string()),
        }
    }
}

impl From<CodeBuilderError> for ToolError {
    fn from(e: CodeBuilderError) -> Self {
        match e {
            CodeBuilderError::LlmPlanRejected(reason) => ToolError::InvalidParams(reason),
            CodeBuilderError::RunTimeout(d) => {
                ToolError::Timeout(format!("CodeBuilder exceeded {d:?}"))
            }
            CodeBuilderError::Cancelled => ToolError::Execution("cancelled".into()),
            CodeBuilderError::UvNotFound => ToolError::Execution(e.to_string()),
            CodeBuilderError::SandboxUnavailable(_) => ToolError::Execution(e.to_string()),
            CodeBuilderError::Unenforceable(_) => ToolError::Execution(e.to_string()),
            CodeBuilderError::SandboxRunFailed(_) => ToolError::Execution(e.to_string()),
            CodeBuilderError::LlmFailed(_) => {
                ToolError::Execution(format!("CodeBuilder LLM call failed: {e}"))
            }
            CodeBuilderError::LlmUnavailable => ToolError::Execution(e.to_string()),
            CodeBuilderError::LlmReturnedNoJson { preview } => {
                ToolError::Execution(format!("CodeBuilder LLM returned non-JSON: {preview}"))
            }
            CodeBuilderError::LlmPlanInvalid(err) => {
                ToolError::Execution(format!("CodeBuilder plan failed schema: {err}"))
            }
            CodeBuilderError::Scratch(s) => {
                ToolError::Execution(format!("CodeBuilder scratch setup: {s}"))
            }
        }
    }
}
