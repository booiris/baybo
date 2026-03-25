pub mod manager;

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::Value;

use aura_core::{Message, OutgoingMessage, Result};

pub use manager::HookManager;

/// Lifecycle points where hooks can be attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookPoint {
    PreMessage,
    PostMessage,
    PreLLMCall,
    PostLLMCall,
    PreToolExecution,
    PostToolExecution,
    PreResponse,
    PostResponse,
    SessionCreated,
    SessionDestroyed,
    CostLimitReached,
    JobStatusChanged,
}

/// The outcome of a hook execution.
#[derive(Debug)]
pub enum HookAction {
    /// Make no changes, continue to the next hook.
    Continue,
    /// Apply field-level modifications to the context, then continue.
    ContinueWith(Box<HookModification>),
    /// Stop the hook chain immediately with the given reason.
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
    pub extra: HashMap<String, Value>,
}

/// Mutable context passed through the hook chain for a given lifecycle point.
pub struct HookContext {
    pub session_id: String,
    pub user_id: Option<String>,
    pub message: Option<Message>,
    pub response: Option<OutgoingMessage>,
    pub job_id: Option<String>,
    pub trace_span_id: Option<String>,
    pub extra: HashMap<String, Value>,
}

impl HookContext {
    /// Apply a modification by merging its fields into this context.
    fn apply(&mut self, modification: HookModification) {
        if let Some(message) = modification.message {
            self.message = Some(message);
        }
        if let Some(response) = modification.response {
            self.response = Some(response);
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

    /// Whether a failure in this hook should abort the main flow.
    /// Defaults to `false` (non-critical).
    fn critical(&self) -> bool {
        false
    }

    /// Execute the hook logic against the provided context.
    async fn execute(&self, ctx: &mut HookContext) -> Result<HookAction>;
}
