use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::HookPoint;

/// Event-specific data carried when a hook is triggered.
///
/// Each variant maps 1:1 to a `HookPoint` and contains only the fields
/// relevant to that event. Use `hook_point()` to get the corresponding
/// `HookPoint` and `matcher_target()` to get the string the matcher
/// evaluates against.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum HookEventData {
    // -- Per-Session Events --
    SessionStart {
        start_method: String,
    },
    SessionEnd {
        exit_reason: String,
    },

    // -- Per-Turn Events --
    UserPromptSubmit {
        prompt: String,
    },
    PreMessage,
    PostMessage,
    Stop,
    StopFailure {
        error_type: String,
        error_message: String,
    },

    // -- LLM Lifecycle Events --
    PreLlmCall {
        provider_name: String,
        model_id: String,
    },
    PostLlmCall {
        provider_name: String,
        model_id: String,
        token_usage: Option<Value>,
    },

    // -- Agentic Loop Events --
    PreToolUse {
        tool_name: String,
        tool_input: Value,
        tool_use_id: String,
    },
    PostToolUse {
        tool_name: String,
        tool_input: Value,
        tool_use_id: String,
        tool_response: Value,
    },
    PostToolUseFailure {
        tool_name: String,
        tool_input: Value,
        tool_use_id: String,
        error: String,
        is_interrupt: bool,
    },
    PermissionRequest {
        tool_name: String,
        tool_input: Value,
        tool_use_id: String,
    },
    PermissionDenied {
        tool_name: String,
        tool_input: Value,
        tool_use_id: String,
        reason: String,
    },

    // -- Response Delivery Events --
    PreResponse,
    PostResponse,

    // -- Subagent Lifecycle Events --
    SubagentStart {
        agent_id: String,
        agent_type: String,
    },
    SubagentStop {
        agent_id: String,
        agent_type: String,
        last_message: Option<String>,
    },

    // -- Task and Job Events --
    TaskCreated {
        task_id: String,
        task_subject: String,
        task_description: Option<String>,
    },
    TaskCompleted {
        task_id: String,
        task_subject: String,
        task_description: Option<String>,
    },
    TeammateIdle {
        agent_id: String,
        agent_type: String,
    },
    JobStatusChanged {
        job_id: String,
        old_status: String,
        new_status: String,
        operation_kind: Option<String>,
    },

    // -- Context Management Events --
    PreCompact {
        trigger: String,
        token_stats: Option<Value>,
    },
    PostCompact {
        trigger: String,
        token_stats: Option<Value>,
    },

    // -- Cost Events --
    CostLimitReached {
        current_cost: f64,
        limit: f64,
    },

    // -- Async / Standalone Events --
    Notification {
        message: String,
        notification_type: String,
    },
    InstructionsLoaded {
        file_path: String,
        load_reason: String,
    },
    ConfigChange {
        config_source: String,
        changed_keys: Vec<String>,
    },
    FileChanged {
        file_path: String,
        change_type: String,
    },
    SkillReloaded {
        skill_name: String,
        skill_version: Option<String>,
        action: String,
    },
    ChannelStatusChanged {
        channel_type: String,
        old_status: String,
        new_status: String,
    },
}

impl HookEventData {
    /// Returns the `HookPoint` this event data corresponds to.
    pub fn hook_point(&self) -> HookPoint {
        match self {
            Self::SessionStart { .. } => HookPoint::SessionStart,
            Self::SessionEnd { .. } => HookPoint::SessionEnd,
            Self::UserPromptSubmit { .. } => HookPoint::UserPromptSubmit,
            Self::PreMessage => HookPoint::PreMessage,
            Self::PostMessage => HookPoint::PostMessage,
            Self::Stop => HookPoint::Stop,
            Self::StopFailure { .. } => HookPoint::StopFailure,
            Self::PreLlmCall { .. } => HookPoint::PreLLMCall,
            Self::PostLlmCall { .. } => HookPoint::PostLLMCall,
            Self::PreToolUse { .. } => HookPoint::PreToolUse,
            Self::PostToolUse { .. } => HookPoint::PostToolUse,
            Self::PostToolUseFailure { .. } => HookPoint::PostToolUseFailure,
            Self::PermissionRequest { .. } => HookPoint::PermissionRequest,
            Self::PermissionDenied { .. } => HookPoint::PermissionDenied,
            Self::PreResponse => HookPoint::PreResponse,
            Self::PostResponse => HookPoint::PostResponse,
            Self::SubagentStart { .. } => HookPoint::SubagentStart,
            Self::SubagentStop { .. } => HookPoint::SubagentStop,
            Self::TaskCreated { .. } => HookPoint::TaskCreated,
            Self::TaskCompleted { .. } => HookPoint::TaskCompleted,
            Self::TeammateIdle { .. } => HookPoint::TeammateIdle,
            Self::JobStatusChanged { .. } => HookPoint::JobStatusChanged,
            Self::PreCompact { .. } => HookPoint::PreCompact,
            Self::PostCompact { .. } => HookPoint::PostCompact,
            Self::CostLimitReached { .. } => HookPoint::CostLimitReached,
            Self::Notification { .. } => HookPoint::Notification,
            Self::InstructionsLoaded { .. } => HookPoint::InstructionsLoaded,
            Self::ConfigChange { .. } => HookPoint::ConfigChange,
            Self::FileChanged { .. } => HookPoint::FileChanged,
            Self::SkillReloaded { .. } => HookPoint::SkillReloaded,
            Self::ChannelStatusChanged { .. } => HookPoint::ChannelStatusChanged,
        }
    }

    /// Returns the string the matcher evaluates against, or `None` if the
    /// event always fires regardless of matcher.
    pub fn matcher_target(&self) -> Option<&str> {
        match self {
            // Per-Session
            Self::SessionStart { start_method } => Some(start_method),
            Self::SessionEnd { exit_reason } => Some(exit_reason),

            // Per-Turn (no matcher for most)
            Self::UserPromptSubmit { .. } => None,
            Self::PreMessage => None,
            Self::PostMessage => None,
            Self::Stop => None,
            Self::StopFailure { error_type, .. } => Some(error_type),

            // LLM Lifecycle
            Self::PreLlmCall { provider_name, .. } => Some(provider_name),
            Self::PostLlmCall { provider_name, .. } => Some(provider_name),

            // Agentic Loop (tool name)
            Self::PreToolUse { tool_name, .. } => Some(tool_name),
            Self::PostToolUse { tool_name, .. } => Some(tool_name),
            Self::PostToolUseFailure { tool_name, .. } => Some(tool_name),
            Self::PermissionRequest { tool_name, .. } => Some(tool_name),
            Self::PermissionDenied { tool_name, .. } => Some(tool_name),

            // Response Delivery
            Self::PreResponse => None,
            Self::PostResponse => None,

            // Subagent Lifecycle
            Self::SubagentStart { agent_type, .. } => Some(agent_type),
            Self::SubagentStop { agent_type, .. } => Some(agent_type),

            // Task and Job
            Self::TaskCreated { .. } => None,
            Self::TaskCompleted { .. } => None,
            Self::TeammateIdle { agent_type, .. } => Some(agent_type),
            Self::JobStatusChanged { new_status, .. } => Some(new_status),

            // Context Management
            Self::PreCompact { trigger, .. } => Some(trigger),
            Self::PostCompact { trigger, .. } => Some(trigger),

            // Cost
            Self::CostLimitReached { .. } => None,

            // Async / Standalone
            Self::Notification {
                notification_type, ..
            } => Some(notification_type),
            Self::InstructionsLoaded { load_reason, .. } => Some(load_reason),
            Self::ConfigChange { config_source, .. } => Some(config_source),
            Self::FileChanged { .. } => None,
            Self::SkillReloaded { skill_name, .. } => Some(skill_name),
            Self::ChannelStatusChanged { channel_type, .. } => Some(channel_type),
        }
    }
}
