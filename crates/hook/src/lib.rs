pub mod error;
pub mod event;
pub mod manager;
pub mod matcher;

use std::collections::HashMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use aura_channels::{Message, OutgoingMessage};

pub use error::HookError;
pub use event::HookEventData;
pub use manager::HookManager;
pub use matcher::HookMatcher;

pub type Result<T> = std::result::Result<T, HookError>;

/// Lifecycle points where hooks can be attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookPoint {
    // Per-Session
    SessionStart,
    SessionEnd,

    // Per-Turn
    UserPromptSubmit,
    PreMessage,
    PostMessage,
    Stop,
    StopFailure,

    // LLM Lifecycle
    PreLLMCall,
    PostLLMCall,

    // Agentic Loop (per tool call)
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PermissionRequest,
    PermissionDenied,

    // Response Delivery
    PreResponse,
    PostResponse,

    // Subagent Lifecycle
    SubagentStart,
    SubagentStop,

    // Task and Job
    TaskCreated,
    TaskCompleted,
    TeammateIdle,
    JobStatusChanged,

    // Context Management
    PreCompact,
    PostCompact,

    // Cost
    CostLimitReached,

    // Async / Standalone
    Notification,
    InstructionsLoaded,
    ConfigChange,
    FileChanged,
    SkillReloaded,
    ChannelStatusChanged,
}

impl HookPoint {
    /// Whether this hook point supports the `Block` action.
    pub fn supports_block(&self) -> bool {
        matches!(
            self,
            Self::UserPromptSubmit
                | Self::Stop
                | Self::PreToolUse
                | Self::PermissionRequest
                | Self::SubagentStop
                | Self::TaskCreated
                | Self::TaskCompleted
                | Self::TeammateIdle
                | Self::Notification
                | Self::ConfigChange
                | Self::SkillReloaded
        )
    }
}

/// The outcome of a hook execution.
#[derive(Debug)]
pub enum HookAction {
    /// Make no changes, continue to the next hook.
    Continue,
    /// Apply field-level modifications to the context, then continue.
    ContinueWith(Box<HookModification>),
    /// Prevent the current action with the given reason.
    /// Only effective for hook points where `supports_block()` is true;
    /// otherwise treated as `Continue` with a warning logged.
    Block(String),
    /// Stop the entire hook chain immediately with the given reason.
    Abort(String),
}

/// Field-level modifications that a hook wants to apply to the context.
///
/// Only fields set to `Some` will overwrite the corresponding context fields.
/// The `extra` map is shallow-merged into the context's existing `extra`.
#[derive(Debug, Default)]
pub struct HookModification {
    pub message: Option<Message>,
    pub response: Option<OutgoingMessage>,
    /// Event-specific field updates (e.g. `updated_input` for `PreToolUse`).
    pub updated_tool_input: Option<Value>,
    pub extra: HashMap<String, Value>,
}

/// Mutable context passed through the hook chain for a given lifecycle point.
pub struct HookContext {
    pub session_id: String,
    pub user_id: Option<String>,
    pub event_data: HookEventData,
    pub message: Option<Message>,
    pub response: Option<OutgoingMessage>,
    pub job_id: Option<String>,
    pub trace_span_id: Option<String>,
    pub extra: HashMap<String, Value>,
}

impl HookContext {
    /// Apply a modification by merging its fields into this context.
    pub(crate) fn apply(&mut self, modification: HookModification) {
        if let Some(message) = modification.message {
            self.message = Some(message);
        }
        if let Some(response) = modification.response {
            self.response = Some(response);
        }
        if let Some(updated_input) = modification.updated_tool_input {
            match &mut self.event_data {
                HookEventData::PreToolUse { tool_input, .. }
                | HookEventData::PermissionRequest { tool_input, .. } => {
                    *tool_input = updated_input;
                }
                _ => {}
            }
        }
        for (key, value) in modification.extra {
            self.extra.insert(key, value);
        }
    }
}

/// A lifecycle hook that can inspect and modify the execution context.
#[async_trait]
pub trait Hook: Send + Sync {
    /// Human-readable name for logging and debugging.
    fn name(&self) -> &str;

    /// The lifecycle point this hook attaches to.
    fn hook_point(&self) -> HookPoint;

    /// Optional matcher to filter when this hook fires.
    /// Returns `None` to match all events at this hook point (equivalent to `HookMatcher::All`).
    fn matcher(&self) -> Option<&HookMatcher> {
        None
    }

    /// Whether a failure in this hook should abort the main flow.
    /// Defaults to `false` (non-critical).
    fn critical(&self) -> bool {
        false
    }

    /// Execute the hook logic against the provided context.
    async fn execute(&self, ctx: &mut HookContext) -> Result<HookAction>;
}

/// Structured output from external (command / HTTP) hook handlers.
///
/// Trait-based hooks return `HookAction` directly. This struct is the
/// intermediate representation for external handlers that communicate
/// via JSON. Adapter implementations convert `HookOutput` to `HookAction`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookOutput {
    /// If `false`, abort the entire agent loop.
    #[serde(default = "default_true")]
    pub r#continue: bool,

    /// Reason when stopping (for logging, not shown to agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,

    /// Event-specific action: `"allow"`, `"block"`, `"deny"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,

    /// Explanation for the decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,

    /// Extra context injected into agent conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,

    /// Warning shown to user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_message: Option<String>,

    /// Omit stdout from debug log.
    #[serde(default)]
    pub suppress_output: bool,

    /// Event-specific structured output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_specific_output: Option<Value>,
}

fn default_true() -> bool {
    true
}

impl HookOutput {
    /// Convert this external handler output into a `HookAction`.
    pub fn into_action(self) -> HookAction {
        if !self.r#continue {
            return HookAction::Abort(
                self.stop_reason
                    .unwrap_or_else(|| "hook requested stop".to_string()),
            );
        }

        match self.decision.as_deref() {
            Some("block") | Some("deny") => {
                HookAction::Block(self.reason.unwrap_or_else(|| "blocked by hook".to_string()))
            }
            _ => {
                let has_modifications = self.additional_context.is_some()
                    || self.system_message.is_some()
                    || self.hook_specific_output.is_some();

                if has_modifications {
                    let mut extra = HashMap::new();
                    if let Some(ctx) = self.additional_context {
                        extra.insert("additional_context".to_string(), Value::String(ctx));
                    }
                    if let Some(msg) = self.system_message {
                        extra.insert("system_message".to_string(), Value::String(msg));
                    }

                    let updated_tool_input = self
                        .hook_specific_output
                        .as_ref()
                        .and_then(|v| v.get("updated_input"))
                        .cloned();

                    HookAction::ContinueWith(Box::new(HookModification {
                        updated_tool_input,
                        extra,
                        ..Default::default()
                    }))
                } else {
                    HookAction::Continue
                }
            }
        }
    }
}
