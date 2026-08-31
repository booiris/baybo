use thiserror::Error;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool not found: {0}")]
    NotFound(String),

    #[error("invalid parameters: {0}")]
    InvalidParams(String),

    #[error("tool execution error: {0}")]
    Execution(String),

    #[error("tool timeout: {0}")]
    Timeout(String),

    #[error("tool not implemented: {0}")]
    NotImplemented(String),

    /// `spawn_subagent` was called from a session whose lineage chain
    /// is already at or beyond the configured maximum subagent depth.
    /// Surfaces directly to the parent LLM as a `ToolError` rather
    /// than running the spawn and returning a `SubagentResult::failed`
    /// so the parent's next turn carries a structured signal.
    #[error("subagent depth exceeded: parent at depth {current_depth}, cap is {cap}")]
    SubagentDepthExceeded { current_depth: u32, cap: u32 },

    /// `spawn_subagent` was called when the trace tree already has
    /// `cap` in-flight subagents under the same root session.
    /// Independent of depth: a wide fan-out is the failure mode this
    /// catches (parent dispatches a hundred siblings in parallel).
    #[error(
        "subagent fan-out exceeded under root session: {current_count} already running, cap is {cap}"
    )]
    SubagentFanoutExceeded { current_count: u32, cap: u32 },

    /// The approval request for this call was refused — by a human answer,
    /// a gate timeout, or a no-prompt-surface policy; `reason` says which.
    #[error("tool '{tool}' permission denied: {reason}")]
    Denied { tool: String, reason: String },

    #[error("mcp error: {0}")]
    Mcp(String),

    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
